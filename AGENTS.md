# AGENTS.md — frankenterm

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## RULE NUMBER 2: ABSOLUTELY NO GIT WORKTREES

**GIT WORKTREES ARE STRICTLY FORBIDDEN IN THIS REPO. DO NOT USE THEM.**

1. **Never run:** `git worktree add`, `git worktree remove`, `git worktree prune`, or any related worktree command.
2. **No exceptions by convenience:** Do not create temporary directories, detached worktrees, or parallel checkout trees for agent work.
3. **Use branches in the main repo only:** All agent work must happen on normal branches in the primary checkout.
4. **If you discover existing worktrees:** stop and report them, then rescue useful commits back into normal branches.

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Git Branch: ONLY Use `main`, NEVER `master`

**The default branch is `main`. The `master` branch exists only for legacy URL compatibility.**

- **All work happens on `main`** — commits, PRs, feature branches all merge to `main`
- **Never reference `master` in code or docs** — if you see `master` anywhere, it's a bug that needs fixing
- **The `master` branch must stay synchronized with `main`** — after pushing to `main`, also push to `master`:
  ```bash
  git push origin main:master
  ```

**If you see `master` referenced anywhere:**
1. Update it to `main`
2. Ensure `master` is synchronized: `git push origin main:master`

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml workspace with `workspace = true` pattern
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]` via `[workspace.lints.rust]`)

### Async Runtime: tokio

This project uses **tokio** for async operations. The `frankenterm` CLI binary and `frankenterm-core` library use tokio for I/O, timers, and task spawning.

**Note:** Some optional features (MCP server mode) bring in `asupersync` and `fastmcp` as dependencies for MCP protocol support, but the primary runtime is tokio.

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (full features) |
| `serde` + `serde_json` | Serialization |
| `toon_rust` | Token-Optimized Object Notation (AI-to-AI format) |
| `clap` | CLI argument parsing |
| `fancy-regex` + `regex` + `aho-corasick` | Pattern matching engine |
| `rusqlite` | Capture storage + FTS5 search |
| `tantivy` | Full-text search |
| `thiserror` + `anyhow` | Error handling |
| `tracing` | Structured logging and diagnostics |
| `wasmtime` | WASM runtime for scripting/extensions (optional) |
| `ftui` | FrankenTUI terminal UI (optional, migration target) |
| `ratatui` + `crossterm` | TUI rendering (optional) |

### Release Profile

The release build optimizes for size (this is a CLI binary):

```toml
[profile.release]
opt-level = "z"     # Optimize for size
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
panic = "abort"     # Smaller binary, no unwinding overhead
strip = true        # Remove debug symbols
```

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `mainV2.rs`
- `main_improved.rs`
- `main_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings (workspace-wide)
cargo check --workspace --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --workspace --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

---

## frankenterm — This Project

**This is the project you're working on.** frankenterm (ft) is a swarm-native terminal platform and control plane for large AI agent fleets.

### What It Does

1. **Runs** a replacement-class terminal runtime focused on massive agent orchestration
2. **Observes** pane/session activity in real-time via delta extraction
3. **Detects** agent state transitions through pattern matching (rate limits, errors, prompts)
4. **Automates** workflows in response to detected events
5. **Enforces** policy-gated actions with auditability and approvals
6. **Exposes** machine-optimized control surfaces (Robot Mode + MCP) for AI-to-AI orchestration

### Strategic Direction

`ft` is not defined by WezTerm integration. The project direction is:

- Replacement of legacy terminal workflows for swarm operations
- Selective design inspiration from Ghostty and Zellij
- Ground-up ft subsystems plus integration/adaptation from `/dp/asupersync`, `/dp/frankensqlite`, and `/frankentui`

### Architecture

```
┌────────────────────────────────────────────────────────────┐
│                      ft (CLI/API)                          │
├────────────────────────────────────────────────────────────┤
│  Robot Mode API    │  Human CLI      │  Watch Daemon       │
│  (ft robot ...)    │  (ft status)    │  (ft watch)         │
├────────────────────────────────────────────────────────────┤
│                     frankenterm-core                       │
│  Pattern Engine │ Capture │ Workflows │ Policy │ Search    │
├────────────────────────────────────────────────────────────┤
│      Backend Adapters (incl. current WezTerm bridge)       │
└────────────────────────────────────────────────────────────┘
```

### Workspace Structure

```
frankenterm/
├── Cargo.toml                         # Workspace root
├── crates/
│   ├── frankenterm/                   # CLI binary (main.rs)
│   └── frankenterm-core/             # Core library
│       └── src/
│           ├── config.rs             # Configuration parsing
│           ├── ingest.rs             # Pane output capture
│           ├── patterns.rs           # Pattern detection engine
│           ├── workflows.rs          # Workflow execution
│           ├── policy.rs             # Safety/access control
│           ├── storage.rs            # SQLite + FTS5
│           └── wezterm.rs            # Terminal backend adapter (current compatibility bridge)
├── frankenterm/                       # In-tree FrankenTerm crates (ex-WezTerm)
│   ├── async_ossl/                   # Async OpenSSL
│   ├── codec/                        # Wire codec
│   ├── config/                       # Config subsystem
│   ├── mux/                          # Multiplexer
│   ├── pty/                          # PTY layer
│   ├── term/                         # Terminal emulator
│   ├── termwiz/                      # Terminal primitives
│   └── ...                           # Additional subsystem crates
├── fuzz/                              # Fuzzing targets
├── docs/                              # Documentation
└── fixtures/                          # Test fixtures
```

### Quick Reference for AI Agents

| Command | Purpose | Output |
|---------|---------|--------|
| `ft robot state` | Get all pane states | JSON/TOON |
| `ft robot get-text <pane_id>` | Read pane content | JSON/TOON |
| `ft robot send <pane_id> "text"` | Send input to pane | JSON/TOON |
| `ft robot wait-for <pane_id> "pattern"` | Wait for pattern match | JSON/TOON |
| `ft robot search "query"` | Full-text search output | JSON/TOON |
| `ft robot events` | Get detection events | JSON/TOON |

**Always use `--format toon` for token-efficient output when processing results with another AI agent.**

### Robot Mode API

The `ft robot` subcommand provides machine-optimized output for AI agents.

#### Output Formats

| Flag | Format | Use Case |
|------|--------|----------|
| `--format json` | JSON | Default, easy parsing |
| `--format toon` | TOON | 40-60% fewer tokens, AI-to-AI |
| `--stats` | Adds stats to stderr | Token savings visibility |

#### Environment Variables

| Variable | Purpose |
|----------|---------|
| `FT_OUTPUT_FORMAT` | Default format (`json` or `toon`) |
| `TOON_DEFAULT_FORMAT` | Fallback default format |
| `FT_WORKSPACE` | Workspace root directory |

**Precedence:** CLI flag > `FT_OUTPUT_FORMAT` > `TOON_DEFAULT_FORMAT` > json

#### State & Discovery

```bash
# Get all panes with their states
ft robot state

# Get pane state (compact TOON, saves ~50% tokens)
ft robot --format toon state

# With token statistics on stderr
ft robot --format toon --stats state
```

**Response envelope:**
```json
{
  "ok": true,
  "data": {
    "panes": [
      {"pane_id": 0, "title": "claude-code", "domain": "local", "cwd": "/project"}
    ]
  }
}
```

#### Reading Pane Content

```bash
# Get recent output from pane
ft robot get-text 0

# Get last N lines (tail)
ft robot get-text 0 --tail 50

# Include escape sequences
ft robot get-text 0 --escapes
```

#### Sending Input

```bash
# Send text to pane (auto-detects paste mode)
ft robot send 1 "/compact"

# Preview without executing
ft robot send 1 "dangerous command" --dry-run

# Send and wait for confirmation pattern
ft robot send 1 "y" --wait-for "confirmed"
```

#### Pattern Waiting

```bash
# Wait for pattern with timeout (seconds)
ft robot wait-for 0 "core.codex:usage_reached" --timeout-secs 3600

# Wait for completion marker
ft robot wait-for 0 "Done" --timeout-secs 60
```

#### Search

```bash
# Full-text search across all captured output
ft robot search "error: compilation failed"

# Filter by pane
ft robot search "rate limit" --pane 0

# Limit results
ft robot search "warning" --limit 5
```

#### Events

```bash
# Get recent detection events
ft robot events --limit 10

# Filter by pane
ft robot events --pane 0

# Filter by rule
ft robot events --rule-id "usage_limit"

# Only unhandled events
ft robot events --unhandled
```

### Session Persistence

| Command | Purpose |
|---------|---------|
| `ft snapshot save` | Capture current mux state (session checkpoint) |
| `ft snapshot list` | List recent snapshots |
| `ft snapshot inspect <id>` | Inspect snapshot contents |
| `ft snapshot diff <id1> <id2>` | Compare two snapshots |
| `ft snapshot delete <id> --force` | Delete a snapshot |
| `ft session list` | List saved sessions |
| `ft session show <session_id>` | Show session + checkpoints |
| `ft session doctor` | Health check for session persistence tables |
| `ft watch` | Startup detection + restore prompt for unclean shutdowns |

Notes:
- `ft snapshot restore` and `ft restart` are currently not fully wired; restore is via `ft watch` on startup.
- Most snapshot/session commands accept `-f json` (auto/plain/json) for machine-friendly output.

### Pattern Rules Tooling

Robot mode includes commands for inspecting and validating pattern rules.

#### List Rules

```bash
# List all rules
ft robot rules list

# Filter by agent type
ft robot rules list --agent-type codex

# Include descriptions
ft robot rules list --verbose
```

#### Test Rules

```bash
# Test text against all rules
ft robot rules test "Usage limit reached. Try again at 2026-01-20 12:34 UTC"

# With full trace
ft robot rules test "some text" --trace
```

#### Show Rule Details

```bash
# Show specific rule
ft robot rules show "codex.usage.reached"
```

#### Lint Rules (Pack Validation)

```bash
# Basic lint (ID naming + regex validation)
ft robot rules lint

# Include fixture coverage check
ft robot rules lint --fixtures

# Strict mode (fail on warnings)
ft robot rules lint --fixtures --strict
```

Lint checks:
- **Naming**: Rule IDs must start with `codex.`, `claude_code.`, `gemini.`, or `wezterm.`
- **Agent type alignment**: Rule ID prefix must match its agent_type field
- **Regex safety**: Warns about nested wildcards (potential ReDoS), excessive length (>500 chars), consecutive spaces
- **Fixture coverage**: Each rule should have at least one corpus fixture (with `--fixtures`)

#### Rule Drift Workflow

When agent output patterns change (new versions, updated prompts), follow this fixture-first workflow:

1. **Capture**: Record the new output that isn't matching
   ```bash
   ft robot get-text <pane_id> --tail 500 > /tmp/new_output.txt
   ```

2. **Add fixture**: Create a minimal test case
   ```bash
   # Copy relevant snippet to corpus
   cp /tmp/new_output.txt crates/frankenterm-core/tests/corpus/<agent>/<event>.txt

   # Create expected output (initially empty to see what matches)
   echo "[]" > crates/frankenterm-core/tests/corpus/<agent>/<event>.expect.json
   ```

3. **Test and iterate**: Run corpus tests to see the diff
   ```bash
   cargo test corpus_fixtures_match_expected
   ```

4. **Update rule**: Modify anchors/regex in the pack definition until the test passes

5. **Validate**: Run the linter to ensure no regressions
   ```bash
   ft robot rules lint --fixtures --strict
   ```

6. **Ship**: Commit the fixture and rule changes together

### Common Agent Workflows

#### 1. Monitor Multiple Agents

```bash
# Start daemon (observe all panes)
ft watch --foreground

# In another terminal: check status
ft robot state

# Wait for any rate limit
ft robot wait-for 0 "usage_reached" --timeout-secs 3600
```

#### 2. Orchestrate Agent Swarm

```bash
# Check all pane states
ft robot --format toon state

# Find pane with error
ft robot search "error" --limit 1

# Send recovery command
ft robot send 0 "/retry"
```

#### 3. Capture and Search

```bash
# Search for specific output across all panes
ft robot search "test failed"

# Get context around match
ft robot get-text 0 --tail 100
```

### Error Handling

Robot mode returns structured errors:

```json
{
  "ok": false,
  "error": {
    "code": "robot.pane_not_found",
    "message": "Pane 99 not found",
    "hint": "Use 'ft robot state' to list available panes"
  }
}
```

Error codes:
- `robot.pane_not_found` - Invalid pane ID
- `robot.timeout` - Wait-for pattern not matched in time
- `robot.wezterm_not_running` - Current compatibility backend is unavailable
- `robot.policy_denied` - Action blocked by safety policy
- `robot.require_approval` - Action requires human approval
- `robot.storage_error` - Database operation failed

### Configuration

Config file: `~/.config/ft/ft.toml` or `$FT_WORKSPACE/.ft/config.toml`

```toml
[general]
log_level = "info"
log_format = "pretty"

[ingest]
poll_interval_ms = 200
min_poll_interval_ms = 50
max_concurrent_captures = 10

[storage]
db_path = "ft.db"
retention_days = 30

[vendored]
mux_socket_path = "/tmp/wezterm.sock"

[vendored.sharding]
enabled = false
socket_paths = ["/tmp/ft-shard-0.sock", "/tmp/ft-shard-1.sock"]
assignment = { strategy = "round_robin" }

[patterns]
enabled_packs = ["builtin:core"]

[workflows]
enabled = true
max_concurrent = 3

[safety]
require_prompt_active = true
block_alt_screen = true
```

### Related Tools

| Tool | Relationship |
|------|--------------|
| `ntm` | Adjacent orchestration tooling; ft is the swarm-native terminal platform |
| `slb` | Simultaneous Launch Button (may integrate with ft workflows) |
| `caam` | Account manager (provides auth for AI agents ft orchestrates) |

---

## Testing

### Testing Policy

Every component crate includes inline `#[cfg(test)]` unit tests alongside the implementation. Tests must cover:
- Happy path
- Edge cases (empty input, max values, boundary conditions)
- Error conditions

Cross-component integration tests live in the workspace `tests/` directory.

### Unit Tests

```bash
# Run all tests across the workspace
cargo test --workspace

# Run with output
cargo test --workspace -- --nocapture

# Run tests for a specific crate
cargo test -p frankenterm
cargo test -p frankenterm-core

# Run specific test by name pattern
cargo test pattern_matching

# Run tests with all features enabled
cargo test --workspace --all-features
```

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** -> `ast-grep`
- Need raw speed or **hunting text** -> `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How does the pattern engine work?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is the robot mode API implemented?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `PatternMatch`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/dp/frankenterm",
  query: "How does the pattern detection engine work?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name -> use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" -> wastes time with manual reads
- **Don't** use `ripgrep` for codemods -> risks collateral edits

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs <changed-files>` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs file.rs file2.rs                    # Specific files (< 1s) — USE THIS
ubs $(git diff --name-only --cached)    # Staged files — before commit
ubs --only=rust,toml src/               # Language filter (3-5x faster)
ubs --ci --fail-on-warning .            # CI mode — before PR
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
Warning  Category (N errors)
    file.rs:42:5 - Issue description
    Suggested fix
Exit code: 1
```

Parse: `file:line:col` -> location | Suggested fix -> how to fix | Exit 0/1 -> pass/fail

### Fix Workflow

1. Read finding -> category + fix suggestion
2. Navigate `file:line:col` -> view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-run `ubs <file>` -> exit 0
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## RCH — Remote Compilation Helper

RCH offloads `cargo build`, `cargo test`, `cargo clippy`, and other compilation commands to a fleet of 8 remote Contabo VPS workers instead of building locally. This prevents compilation storms from overwhelming csd when many agents run simultaneously.

**RCH is installed at `~/.local/bin/rch` and is hooked into Claude Code's PreToolUse automatically.** Most of the time you don't need to do anything if you are Claude Code — builds are intercepted and offloaded transparently.

To manually offload a build:
```bash
rch exec -- cargo build --release
rch exec -- cargo test
rch exec -- cargo clippy
```

Quick commands:
```bash
rch doctor                    # Health check
rch workers probe --all       # Test connectivity to all 8 workers
rch status                    # Overview of current state
rch queue                     # See active/waiting builds
```

If rch or its workers are unavailable, it fails open — builds run locally as normal.

**Note for Codex/GPT-5.2:** Codex does not have the automatic PreToolUse hook, but you can (and should) still manually offload compute-intensive compilation commands using `rch exec -- <command>`. This avoids local resource contention when multiple agents are building simultaneously.

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |
| `--robot-label-flow` | Cross-label dependency: `flow_matrix`, `dependencies`, `bottleneck_labels` |
| `--robot-label-attention [--attention-limit=N]` | Attention-ranked labels |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-history` | Bead-to-commit correlations |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--robot-burndown <sprint>` | Sprint burndown, scope changes, at-risk items |
| `--robot-forecast <id\|all>` | ETA predictions with dependency-aware scheduling |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |
| `--export-graph <file.html>` | Interactive HTML visualization |

### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank
bv --robot-triage --robot-triage-by-track    # Group by parallel work streams
bv --robot-triage --robot-triage-by-label    # Group by domain
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

<!-- bv-agent-instructions-v1 -->

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

**Important:** `br` is non-invasive—it NEVER executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync --flush-only  # Export to JSONL (NO git operations)
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything together
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress -> closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `br sync --flush-only && git add .beads/` before ending session

<!-- end-bv-agent-instructions -->

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Sync beads** - `br sync --flush-only` to export to JSONL
5. **Hand off** - Provide context for next session

---

Note for Codex/GPT-5.2:

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/cli/commands/upgrade.rs, src/storage/sqlite.rs, tests/conformance.rs, tests/storage_deps.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
2. Triage beads_rust-orko (clippy/cargo warnings) and beads_rust-ydqr (rustfmt failures).
3. If you want a full suite run later, fix conformance/clippy blockers and re-run cargo test --all.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurrence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into thinking YOU made the changes and simply don't recall it for some reason.

---

## Note on Built-in TODO Functionality

Also, if I ask you to explicitly use your built-in TODO functionality, don't complain about this and say you need to use beads. You can use built-in TODOs if I tell you specifically to do so. Always comply with such orders.
