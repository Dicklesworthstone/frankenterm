# AGENTS.md — ft (FrankenTerm)

> Guidelines for AI coding agents working in this Rust codebase.

---

## Quick Reference for AI Agents

| Command | Purpose | Output |
|---------|---------|--------|
| `ft robot state` | Get all pane states | JSON/TOON |
| `ft robot get-text <pane_id>` | Read pane content | JSON/TOON |
| `ft robot send <pane_id> "text"` | Send input to pane | JSON/TOON |
| `ft robot wait-for <pane_id> "pattern"` | Wait for pattern match | JSON/TOON |
| `ft robot search "query"` | Full-text search output | JSON/TOON |
| `ft robot events` | Get detection events | JSON/TOON |

**Always use `--format toon` for token-efficient output when processing results with another AI agent.**

---

## Session Persistence (Quick Reference)

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

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created. You MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.

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
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`) before considering a destructive command.

---

## What ft Does

**ft (FrankenTerm)** is a swarm-native terminal platform and control plane for large AI agent fleets. It:

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

### Core Architecture

```
┌────────────────────────────────────────────────────────────┐
│                      ft (CLI/API)                          │
├────────────────────────────────────────────────────────────┤
│  Robot Mode API    │  Human CLI      │  Watch Daemon       │
│  (ft robot ...)    │  (ft status)    │  (ft watch)         │
├────────────────────────────────────────────────────────────┤
│                     frankenterm-core                      │
│  Pattern Engine │ Capture │ Workflows │ Policy │ Search    │
├────────────────────────────────────────────────────────────┤
│      Backend Adapters (incl. current WezTerm bridge)      │
└────────────────────────────────────────────────────────────┘
```

---

## Robot Mode API

The `ft robot` subcommand provides machine-optimized output for AI agents.

### Output Formats

| Flag | Format | Use Case |
|------|--------|----------|
| `--format json` | JSON | Default, easy parsing |
| `--format toon` | TOON | 40-60% fewer tokens, AI-to-AI |
| `--stats` | Adds stats to stderr | Token savings visibility |

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `FT_OUTPUT_FORMAT` | Default format (`json` or `toon`) |
| `TOON_DEFAULT_FORMAT` | Fallback default format |
| `FT_WORKSPACE` | Workspace root directory |

**Precedence:** CLI flag > `FT_OUTPUT_FORMAT` > `TOON_DEFAULT_FORMAT` > json

### Commands

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
ft robot wait-for 0 "✓ Done" --timeout-secs 60
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

---

## Toolchain: Rust & Cargo

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Unsafe code:** Forbidden (via `[workspace.lints.rust]` in Cargo.toml)
- **Workspace:** Multi-crate (frankenterm, frankenterm-core, fuzz)

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `serde` + `serde_json` | Serialization |
| `toon_rust` | Token-Optimized Object Notation |
| `tokio` | Async runtime |
| `clap` | CLI argument parsing |
| `fancy-regex` | Advanced pattern matching |
| `rusqlite` | Capture storage + FTS5 search |

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Make code changes manually.

### No File Proliferation

**NEVER** create variations like `mainV2.rs` or `main_improved.rs`. Revise existing files in place.

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors
cargo check --all-targets

# Check for clippy lints (pedantic + nursery enabled)
cargo clippy --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

---

## Testing

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test by name pattern
cargo test pattern_matching
```

---

## Pattern Rules Tooling

Robot mode includes commands for inspecting and validating pattern rules.

### List Rules

```bash
# List all rules
ft robot rules list

# Filter by agent type
ft robot rules list --agent-type codex

# Include descriptions
ft robot rules list --verbose
```

### Test Rules

```bash
# Test text against all rules
ft robot rules test "Usage limit reached. Try again at 2026-01-20 12:34 UTC"

# With full trace
ft robot rules test "some text" --trace
```

### Show Rule Details

```bash
# Show specific rule
ft robot rules show "codex.usage.reached"
```

### Lint Rules (Pack Validation)

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

### Rule Drift Workflow

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

---

## Common Agent Workflows

### 1. Monitor Multiple Agents

```bash
# Start daemon (observe all panes)
ft watch --foreground

# In another terminal: check status
ft robot state

# Wait for any rate limit
ft robot wait-for 0 "usage_reached" --timeout-secs 3600
```

### 2. Orchestrate Agent Swarm

```bash
# Check all pane states
ft robot --format toon state

# Find pane with error
ft robot search "error" --limit 1

# Send recovery command
ft robot send 0 "/retry"
```

### 3. Capture and Search

```bash
# Search for specific output across all panes
ft robot search "test failed"

# Get context around match
ft robot get-text 0 --tail 100
```

---

## Error Handling

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

---

## Configuration

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

---

## Project Structure

```
frankenterm/
├── crates/
│   ├── frankenterm/  # CLI binary (main.rs ~31k lines)
│   └── frankenterm-core/      # Core library
│       └── src/
│           ├── config.rs      # Configuration parsing
│           ├── ingest.rs      # Pane output capture
│           ├── patterns.rs    # Pattern detection engine
│           ├── workflows.rs   # Workflow execution
│           ├── policy.rs      # Safety/access control
│           ├── storage.rs     # SQLite + FTS5
│           └── wezterm.rs     # Terminal backend adapter (current compatibility bridge)
├── fuzz/             # Fuzzing targets
├── docs/             # Documentation
└── fixtures/         # Test fixtures
```

---

## Related Tools

| Tool | Relationship |
|------|--------------|
| `ntm` | Adjacent orchestration tooling; ft is the swarm-native terminal platform |
| `slb` | Simultaneous Launch Button (may integrate with ft workflows) |
| `caam` | Account manager (provides auth for AI agents ft orchestrates) |

---

## Version

Generated for ft v0.1.0 (2026-01-25)

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
