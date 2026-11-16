# Comprehensive Analysis of cross_agent_session_resumer

> Analysis document for FrankenTerm bead `ft-2vuw7.27.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: Research pass ft-2vuw7.27.1

---

## R1: Repository Topology and Crate/Module Boundary Inventory

### Overview

`cross_agent_session_resumer` (binary name: `casr`) is a single-crate Rust project that converts AI coding agent sessions between providers. It reads session data from one provider's native format, normalizes it into a canonical intermediate representation (IR), then writes it back in another provider's native format. The repository lives at `/dp/cross_agent_session_resumer`.

**Total source**: ~18,720 LOC across `src/`, ~15,470 LOC in integration tests (`tests/`), plus ~1,149 LOC in `install.sh` and shell test scripts. Grand total: ~35,000+ LOC.

### Crate: `casr` (single crate)

| File | LOC | Purpose |
|------|-----|---------|
| `src/lib.rs` | 12 | Library entry point; re-exports `discovery`, `error`, `model`, `pipeline`, `providers`. `#![forbid(unsafe_code)]` enforced. |
| `src/main.rs` | 607 | CLI binary: `clap` subcommand dispatch (`Resume`, `List`, `Info`, `Providers`, `Completions`), colored output, `--json` machine mode. |
| `src/model.rs` | 617 | **Canonical IR**: `CanonicalSession`, `CanonicalMessage`, `MessageRole`, `ToolCall`, `ToolResult` plus helpers: `flatten_content()`, `parse_timestamp()`, `normalize_role()`, `truncate_title()`, `reindex_messages()`. |
| `src/error.rs` | 245 | `CasrError` enum with 8 actionable error variants: `SessionNotFound`, `AmbiguousSessionId`, `UnknownProviderAlias`, `ProviderUnavailable`, `SessionReadError`, `SessionWriteError`, `SessionConflict`, `ValidationError`, `VerifyFailed`. |
| `src/discovery.rs` | 728 | `ProviderRegistry` (central registry of all providers), `SourceHint` parsing (`--source` flag), `ResolvedSession`, `DetectionResult`. Multi-step session resolution algorithm with file-signature inference for out-of-tree files. |
| `src/pipeline.rs` | 1,095 | `ConversionPipeline` orchestrator: detect -> read -> validate -> enrich -> write -> verify. `atomic_write()` for safe file operations with temp-then-rename semantics. `validate_session()` for pre-write checks. |
| `src/providers/mod.rs` | 92 | `Provider` trait definition (object-safe, `Send + Sync`), `WriteOptions`, `WrittenSession` types. |

### Provider Implementations (14 providers)

| Provider Module | LOC | Native Format | CLI Alias | Storage Location |
|-----------------|-----|---------------|-----------|------------------|
| `claude_code.rs` | 897 | JSONL | `cc` | `~/.claude/projects/<key>/<session-id>.jsonl` |
| `codex.rs` | 1,146 | JSONL (modern) / JSON (legacy) | `cod` | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` |
| `gemini.rs` | 899 | JSON | `gmi` | `~/.gemini/tmp/<sha256-hash>/chats/session-*.json` |
| `cursor.rs` | 1,498 | SQLite `state.vscdb` | `cur` | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| `cline.rs` | 1,930 | JSON (VS Code ext storage) | `cln` | `<host>/User/globalStorage/saoudrizwan.claude-dev/tasks/<id>/api_conversation_history.json` |
| `aider.rs` | 1,199 | Markdown | `aid` | `.aider.chat.history.md` (per-project) |
| `amp.rs` | 1,078 | JSON | `amp` | `~/.local/share/amp/threads/<thread-id>.json` or VS Code globalStorage |
| `opencode.rs` | 1,535 | SQLite `opencode.db` | `opc` | `.opencode/opencode.db` (per-project) |
| `chatgpt.rs` | 1,358 | JSON (tree-based `mapping`) | `gpt` | `~/Library/Application Support/com.openai.chat/conversations-*/` |
| `clawdbot.rs` | 633 | JSONL (bare `{role, content}`) | `cwb` | `~/.clawdbot/sessions/*.jsonl` |
| `vibe.rs` | 600 | JSONL (flexible field names) | `vib` | `~/.vibe/logs/session/*/messages.jsonl` |
| `factory.rs` | 746 | JSONL (typed entries) | `fac` | `~/.factory/sessions/<workspace-slug>/<uuid>.jsonl` |
| `openclaw.rs` | 862 | JSONL (content blocks) | `ocl` | `~/.openclaw/agents/openclaw/sessions/*.jsonl` |
| `pi_agent.rs` | 943 | JSONL (session + message typed entries) | `pi` | `~/.pi/agent/sessions/<safe-path>/<timestamp>_<uuid>.jsonl` |

### Test Files (18 integration test files)

| Test File | LOC | Purpose |
|-----------|-----|---------|
| `roundtrip_test.rs` | 2,463 | Full round-trip fidelity matrix: `read(write(read(source)))` across all provider pairs |
| `writer_test.rs` | 2,009 | Writer correctness for each provider's native format |
| `cass_parity_test.rs` | 1,415 | Regression tests verifying behavioral parity with CASS (the upstream search tool) |
| `pipeline_test.rs` | 1,291 | Pipeline orchestration: validate/enrich/convert end-to-end |
| `cli_e2e_test.rs` | 1,285 | CLI binary smoke tests via `assert_cmd` |
| `golden_output_test.rs` | 1,225 | Golden file comparison for all fixture -> canonical mappings |
| `json_contract_test.rs` | 979 | JSON output format contract stability |
| `error_paths_test.rs` | 845 | Error path coverage: malformed input, missing providers, ambiguous sessions |
| `atomic_write_test.rs` | 815 | Failure injection matrix for `atomic_write()` |
| `fixtures_test.rs` | 640 | Fixture manifest validation |
| `scalability_test.rs` | 437 | Performance regression gates with timing budgets |
| `discovery_test.rs` | 447 | Provider detection and session resolution logic |
| `verbose_trace_test.rs` | 355 | Tracing output verification |
| `malformed_input_test.rs` | 335 | Malformed input handling across providers |
| `trace_event_test.rs` | 294 | Event tracing instrumentation |
| `corrupted_sqlite_test.rs` | 276 | Corrupted SQLite handling for Cursor/OpenCode |
| `invalid_session_id_test.rs` | 253 | Invalid session ID error paths |
| `test_env.rs` | 106 | Reentrant environment lock for test parallelism safety |

### Test Fixtures

27 fixture files organized by provider in `tests/fixtures/`, with 27 corresponding expected-output JSON files in `tests/fixtures/expected/`. A `fixtures_manifest.json` catalogs each fixture with its provider, format, intent, and expected output path.

---

## R2: Build/Runtime/Dependency Map and Feature-Flag Matrix

### Feature Flags

The project has **no feature flags**. All 14 providers are unconditionally compiled. The release profile uses `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true` for minimal binary size.

### Rust Edition and Toolchain

- **Edition**: 2024 (specified in `Cargo.toml`)
- **Toolchain**: Nightly (specified in `rust-toolchain.toml` as `channel = "nightly"`)
- **MSRV**: Not formally specified; nightly-only due to Rust 2024 edition features (let-chains, `if let` in match guards, etc.)

### Runtime Dependencies (13)

| Crate | Version | Purpose |
|-------|---------|---------|
| `anyhow` | 1 | Error propagation in pipeline/providers |
| `chrono` | 0.4 (features: `clock`, `serde`) | Timestamp parsing and formatting |
| `clap` | 4 (features: `derive`, `env`) | CLI argument parsing |
| `clap_complete` | 4 | Shell completion generation (bash/zsh/fish) |
| `colored` | 3 | Terminal colored output |
| `dirs` | 6 | Platform-appropriate home/config/data directory resolution |
| `glob` | 0.3 | File pattern matching |
| `rusqlite` | 0.33 (feature: `bundled`) | SQLite access for Cursor and OpenCode providers |
| `serde` | 1 (feature: `derive`) | Serialization framework |
| `sha2` | 0.10 | SHA-256 for Gemini project directory hashing |
| `serde_json` | 1 | JSON parsing and generation |
| `thiserror` | 2 | Typed error derivation |
| `uuid` | 1 (feature: `v4`) | Session ID generation |
| `walkdir` | 2 | Recursive directory traversal |
| `tracing` | 0.1 | Structured logging |
| `tracing-subscriber` | 0.3 (features: `env-filter`, `fmt`) | Log formatting and filtering |
| `urlencoding` | 2 | URL encoding for Cursor virtual session paths |
| `which` | 7 | Binary detection in PATH |

### Dev Dependencies (3)

| Crate | Version | Purpose |
|-------|---------|---------|
| `assert_cmd` | 2 | CLI binary testing |
| `predicates` | 3 | Assertion predicates for test output |
| `tempfile` | 3 | Temporary directory management in tests |

### Build Dependencies (1)

| Crate | Version | Purpose |
|-------|---------|---------|
| `vergen-gix` | 9 (features: `build`, `cargo`, `rustc`) | Embeds build timestamp, git SHA, target triple into `--version` output |

### CI Pipeline

The CI (`.github/workflows/ci.yml`) runs 7 parallel jobs:
1. **Check/Lint/Test**: `cargo fmt --check`, `cargo clippy`, `cargo test --lib`, CASS independence guardrail
2. **Integration Tests**: `cargo test --tests` (4 threads)
3. **Test Report (JSON)**: Generates machine-readable JSONL test report
4. **Coverage (llvm-cov)**: Enforces 70% overall, 80% for `model.rs` and `pipeline.rs`
5. **Roundtrip Fidelity Matrix**: Full provider-pair round-trip tests
6. **Perf Regression Gates**: Scalability test with timing budgets
7. **E2E Shell Tests**: Full shell-level e2e tests
8. **Release Build**: Verifies release profile builds cleanly

---

## R3: Public Surface Inventory (APIs/CLI/MCP/Config/Events)

### CLI Commands

Binary name: `casr`

| Command | Syntax | Description |
|---------|--------|-------------|
| `resume` | `casr <target-alias> resume <session-id> [--dry-run] [--force] [--source <alias-or-path>] [--enrich]` | Convert session from source provider to target provider format |
| `list` | `casr list [--provider <slug>] [--workspace <path>] [--limit N] [--sort date\|messages\|provider]` | List all discoverable sessions across installed providers |
| `info` | `casr info <session-id>` | Show detailed metadata for a single session |
| `providers` | `casr providers` | List detected providers and their installation status |
| `completions` | `casr completions <shell>` | Generate shell completions (bash, zsh, fish) |

Global flags: `--verbose`, `--trace`, `--json`

### Provider Trait (`src/providers/mod.rs`)

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;                    // Human-readable name
    fn slug(&self) -> &str;                    // Short slug for metadata
    fn cli_alias(&self) -> &str;               // CLI alias for subcommands
    fn detect(&self) -> DetectionResult;       // Probe installation status
    fn session_roots(&self) -> Vec<PathBuf>;   // Root directories for sessions
    fn owns_session(&self, session_id: &str) -> Option<PathBuf>;  // Check ownership
    fn read_session(&self, path: &Path) -> anyhow::Result<CanonicalSession>;  // Read native -> IR
    fn write_session(&self, session: &CanonicalSession, opts: &WriteOptions) -> anyhow::Result<WrittenSession>;  // Write IR -> native
    fn resume_command(&self, session_id: &str) -> String;  // Shell command to resume
    fn list_sessions(&self) -> Option<Vec<(String, PathBuf)>>;  // Optional: enumerate all sessions
}
```

### Canonical IR Types (`src/model.rs`)

**`CanonicalSession`** -- the central IR type:
- `session_id: String` -- provider-assigned or generated UUID
- `provider_slug: String` -- origin provider (e.g. `"claude-code"`)
- `workspace: Option<PathBuf>` -- project root directory
- `title: Option<String>` -- first user message or explicit title
- `started_at: Option<i64>` / `ended_at: Option<i64>` -- epoch milliseconds
- `messages: Vec<CanonicalMessage>` -- ordered conversation
- `metadata: serde_json::Value` -- provider-specific extras
- `source_path: PathBuf` -- original session file
- `model_name: Option<String>` -- most common model in session

**`CanonicalMessage`**:
- `idx: usize`, `role: MessageRole`, `content: String`, `timestamp: Option<i64>`
- `author: Option<String>`, `tool_calls: Vec<ToolCall>`, `tool_results: Vec<ToolResult>`
- `extra: serde_json::Value` -- provider-specific preserved fields

**`MessageRole`**: `User | Assistant | Tool | System | Other(String)`

**`ToolCall`**: `{ id: Option<String>, name: String, arguments: serde_json::Value }`

**`ToolResult`**: `{ call_id: Option<String>, content: String, is_error: bool }`

### Support Types

- **`WriteOptions`**: `{ force: bool }` -- controls overwrite behavior
- **`WrittenSession`**: `{ paths: Vec<PathBuf>, session_id: String, resume_command: String, backup_path: Option<PathBuf> }`
- **`DetectionResult`**: `{ installed: bool, version: Option<String>, evidence: Vec<String> }`
- **`ConvertOptions`**: `{ dry_run, force, verbose, enrich, source_hint }`
- **`ConversionResult`**: `{ source_provider, target_provider, canonical_session, written: Option<WrittenSession>, warnings }`
- **`ValidationResult`**: `{ errors: Vec<String>, warnings: Vec<String>, info: Vec<String> }`
- **`SourceHint`**: `Alias(String) | Path(PathBuf)` -- parsed from `--source` CLI flag
- **`CasrError`**: 8-variant typed error enum with structured context for JSON error output

### Public Helper Functions

- `flatten_content(value: &Value) -> String` -- Normalizes heterogeneous content representations (strings, block arrays, objects) into plain text
- `parse_timestamp(value: &Value) -> Option<i64>` -- Parses seconds, millis, floats, ISO-8601 into epoch millis
- `normalize_role(role_str: &str) -> MessageRole` -- Maps provider-specific role strings to canonical roles
- `truncate_title(text: &str, max_len: usize) -> String` -- First-line extraction with char-boundary-safe truncation
- `reindex_messages(messages: &mut [CanonicalMessage])` -- Re-assigns sequential 0-based indices
- `atomic_write(target_path, content, force, provider_slug) -> Result<AtomicWriteOutcome, CasrError>` -- Safe file writing with temp-then-rename
- `validate_session(session: &CanonicalSession) -> ValidationResult` -- Pre-write quality checks
- `restore_backup(outcome, provider_slug) -> Result<(), CasrError>` -- Undo after verification failure

### No MCP / No Config Files / No Events

casr has no MCP integration, no configuration files, and no event system. All configuration is via CLI flags and environment variables. Each provider respects a `<PROVIDER>_HOME` env var for overriding default storage paths.

---

## R4: Execution-Flow Tracing Across Core Workflows

### Session Conversion Pipeline (`ConversionPipeline::convert()`)

The full pipeline in `src/pipeline.rs` executes 9 deterministic steps:

**Step 1: Resolve target provider**
- `registry.find_by_alias(target_alias)` looks up the target by CLI alias or slug
- Returns `CasrError::UnknownProviderAlias` if not found

**Step 2: Detect target provider installation**
- `target_provider.detect()` probes for binary in PATH and config directories
- Non-fatal: if not installed, a warning is added but conversion continues (filesystem-only writes are still valid)

**Step 3: Resolve source session**
- `registry.resolve_session(session_id, source_hint)` implements a 3-branch algorithm:
  - **Path hint**: Bypass discovery, identify provider by session-root containment or file-signature inference
  - **Alias hint**: Search only the specified provider via `owns_session()`
  - **Auto**: Search ALL installed providers via `owns_session()`, detect ambiguity (multiple matches -> `AmbiguousSessionId`)

**Step 4: Read source into canonical IR**
- `resolved.provider.read_session(&resolved.path)` parses native format into `CanonicalSession`
- Each provider has its own parsing logic handling format quirks (content blocks, nested messages, SQLite queries, Markdown parsing, etc.)

**Step 5: Validate canonical session**
- `validate_session(&canonical)` checks for:
  - **Errors** (fatal): empty messages, missing user/assistant role
  - **Warnings** (non-fatal): missing workspace, no timestamps, consecutive same-role messages, very short session
  - **Info**: tool call presence, orphaned tool result IDs

**Step 6: Optional enrichment** (`--enrich` flag)
- `prepend_enrichment_messages()` inserts 2 synthetic `System` messages at index 0:
  1. Conversion notice: source/target provider, original session ID, workspace
  2. Recent summary: compact summary of last 4 messages (max 180 chars each)
- Both marked with `extra.casr_enrichment = true` and `extra.synthetic = true`

**Step 7: Same-provider short-circuit**
- If source and target are the same provider (without `--enrich`), skip write and return existing session info with resume command

**Step 8: Write to target format**
- `target_provider.write_session(&canonical, &write_opts)` serializes IR to native format
- All providers use `atomic_write()` for safe file operations:
  - Create parent directories
  - Check for conflicts (existing file + no `--force` -> `SessionConflict`)
  - If `--force`: rename existing to `.bak`
  - Write to temp file (`.casr-tmp-<uuid>`) with `fsync`
  - Atomic rename temp -> target
  - On any failure: clean temp, restore `.bak` backup

**Step 9: Read-back verification**
- `target_provider.read_session(written_path)` re-reads the written file
- `readback_mismatch_detail()` compares: message count, per-message role, per-message content
- If mismatch: `rollback_written_session()` removes the broken output and restores backup
- Returns `CasrError::VerifyFailed` on verification failure (indicates a writer bug)

### Provider Detection Flow

Each provider's `detect()` method checks:
1. Binary in PATH via `which::which("<binary-name>")`
2. Config/data directory existence (platform-specific via `dirs` crate)
3. Returns `DetectionResult { installed, version, evidence }` with diagnostic strings

### File Signature Inference

When `--source <path>` points to a file outside any known provider root, `discovery.rs` performs lightweight signature inference:
- `.vscdb` extension -> Cursor
- `.jsonl` first-line heuristics: `type: "session_meta"` -> Codex, `sessionId + uuid + cwd` -> Claude Code, `role + content` (no type) -> ClawdBot, etc.
- `.json` key heuristics: `sessionId + messages` -> Gemini, `mapping` -> ChatGPT, `session` -> Codex
- Fallback: probe ALL providers' `read_session()`, pick the one with most messages and plausible role distribution

---

## R5: Persistence, State, and Data Contracts

### Session Format Schemas by Provider

**Claude Code (JSONL)**: Each line has `type` ("user"/"assistant"/"file-history-snapshot"/"summary"), `sessionId`, `uuid`, `cwd`, `timestamp`, `message.role`, `message.content` (string or array of content blocks), `message.model`.

**Codex (JSONL modern)**: Typed envelope `{ type: "session_meta"|"response_item"|"event_msg", timestamp, payload }`. Session meta: `payload.id`, `payload.cwd`. Response items: `payload.role`, `payload.content`. Event msgs: sub-typed as `user_message`, `agent_reasoning`, `token_count`.

**Codex (JSON legacy)**: Single object `{ session: { id, cwd }, items: [{ role, content, timestamp }] }`.

**Gemini CLI (JSON)**: `{ sessionId, startTime, lastUpdated, messages: [{ type: "user"|"gemini"|"model", content, timestamp }] }`. Project directory keyed by `SHA256(workspace_path)`.

**Cursor (SQLite)**: `cursorDiskKV` table: `composerData:<uuid>` keys hold session metadata + message ordering; `bubbleId:<composerId>:<bubbleId>` keys hold individual message data. Numeric type 1 = User, 2 = Assistant.

**Cline (JSON tree)**: `api_conversation_history.json` in `tasks/<taskId>/`, plus `taskHistory.json` for metadata. Content includes tool use blocks.

**Aider (Markdown)**: `# aider chat started at <timestamp>` headers delimit sessions. `#### <text>` = user messages. `> <text>` = tool output. Bare text = assistant responses.

**Amp (JSON)**: Single JSON file per thread with `role` and `content` arrays containing typed blocks (`text`, `tool_use`, `tool_result`).

**OpenCode (SQLite)**: `sessions` table (id, title, created_at) + `messages` table (id, session_id, role, content, created_at) + `files` table.

**ChatGPT (JSON tree)**: `mapping` object with node IDs as keys, each containing `message.author.role`, `message.content.parts`, `parent` pointer for tree traversal. Float timestamps.

**ClawdBot (JSONL bare)**: Simplest format: `{ role, content, timestamp }` per line. No session header, no content blocks.

**Vibe (JSONL flexible)**: Multiple field name conventions for role (`role`, `speaker`, `message.role`), content (`content`, `text`, `message.content`), and timestamp (`timestamp`, `created_at`, `ts`, etc.).

**Factory (JSONL typed)**: `session_start` header with `id`, `title`, `cwd`, then `message` entries with nested `message.role`, `message.content`, `message.model`. Workspace encoded in parent directory slug.

**OpenClaw (JSONL blocks)**: `session` header, `message` entries with content as array of typed blocks (`text`, `toolCall`, `thinking`).

**Pi-Agent (JSONL blocks)**: Similar to OpenClaw with `session` header (includes `provider`, `modelId`), `message` entries, `model_change` entries. Filename must contain underscore for recognition.

### CanonicalSession Contract

The canonical IR serves as the Rosetta Stone: every provider reads INTO it and writes FROM it. Key invariants:
- `messages` are always 0-indexed sequentially after `reindex_messages()`
- `timestamp` values are always epoch milliseconds when present
- Empty/whitespace-only content messages are filtered during read
- `extra` field preserves provider-specific data for round-trip fidelity
- `metadata` at session level holds provider-wide extras

### Round-Trip Fidelity Expectations

| Field | Expectation |
|-------|-------------|
| `message_count` | EXACT |
| `message_roles` | EXACT |
| `message_content` | EXACT (text-only) |
| `session_id` | NEW (generated UUID for target) |
| `workspace` | EXACT for CC/Cod; BEST-EFFORT for Gemini |
| `model_name` | EXACT for CC targets; absent for Cod/Gmi |
| `git_branch` | LOST when leaving Claude Code |
| `token_usage` | LOST when leaving Codex |
| `citations` | LOST when leaving Gemini |

---

## R6: Reliability, Performance, and Security Posture

### Error Handling

- **Typed errors**: `CasrError` (8 variants) with `thiserror` derive, each carrying diagnostic context (paths, provider names, scanned counts)
- **Actionable messages**: Every error suggests next steps (e.g. "Run `casr list`", "Use `--source`", "Use `--force`")
- **JSON error output**: `--json` mode produces `{ ok: false, error_type: "SessionNotFound", message: "..." }` for machine consumption
- **Malformed input tolerance**: Invalid JSON lines in JSONL files are skipped with tracing warnings; valid lines are preserved
- **Validation tiers**: Errors (fatal, pipeline stops), Warnings (non-fatal, surfaced to user), Info (verbose/trace only)
- **Rollback on verification failure**: If read-back verification fails, written files are deleted and backups restored

### Atomic Write Safety

`atomic_write()` in `pipeline.rs` guarantees:
- **No partial writes**: Content is written to a temp file (`.casr-tmp-<uuid>`), fsynced, then atomically renamed
- **Backup on force**: Existing files are renamed to `.bak` before overwrite; deduplicated with `.bak.1`, `.bak.2`, etc.
- **Cleanup on failure**: Temp files are always cleaned up; backups are restored on any write/rename failure
- **Failure injection testing**: `AtomicWriteFailStage` enum enables test-only fault injection at 6 stages (BackupRename, TempFileCreate, WriteAll, Flush, SyncAll, FinalRename)

### Performance Characteristics

- **Scalability tests**: CI enforces timing budgets via `tests/scalability_test.rs` and `CASR_PERF_METRICS_FILE`
- **Reader throughput**: CI validates `min_reader_throughput_msg_per_sec` for each provider
- **Discovery latency**: CI validates `found_elapsed_ms` and `miss_elapsed_ms` against budgets
- **Single-threaded I/O**: All operations are synchronous; no async runtime. This is appropriate for a CLI tool.
- **Directory walking depth**: `walkdir` is capped at `max_depth(4)` to avoid scanning deep trees

### Security Considerations

- **`#![forbid(unsafe_code)]`**: Enforced in both `lib.rs` and `main.rs`
- **No network access**: casr operates exclusively on local files; no HTTP requests, no API calls
- **No secrets handling**: Session content may contain API keys/tokens in conversation history, but casr does not parse or extract them
- **File permissions**: No explicit permission management; relies on OS defaults
- **SQLite read-only**: Cursor and OpenCode databases are opened with `SQLITE_OPEN_READ_ONLY` flag
- **Env var overrides**: Every provider respects `<PROVIDER>_HOME` env vars, which could redirect reads/writes to arbitrary paths
- **ChatGPT encryption**: v2/v3 encrypted conversations are explicitly skipped (only v1 unencrypted supported)

### Test Coverage

- **CI enforces**: 70% overall src coverage, 80% for `model.rs` and `pipeline.rs`
- **27 fixture files** with golden expected outputs
- **Round-trip matrix**: Tests every provider pair combination
- **CASS parity tests**: 10+ divergence tracking tests documenting intentional behavioral differences from the upstream CASS project
- **Corrupted input**: Tests for corrupted SQLite files, malformed JSON, invalid session IDs

---

## R7: Integration Seams and Extraction Candidates

### Natural Integration Points with FrankenTerm

**1. FrankenTerm Flight Recorder as a casr Provider**

FrankenTerm's flight recorder (`recorder_*.rs` modules) captures terminal session data. A natural integration would be implementing the `Provider` trait for FrankenTerm's recorder format, enabling:
- Export: Convert FrankenTerm-recorded sessions into Claude Code / Codex / Gemini format for resumption
- Import: Read sessions from other providers and inject context into FrankenTerm's session management

The `Provider` trait methods that would need implementation:
- `detect()`: Check for FrankenTerm recorder data directories
- `session_roots()`: Return flight recorder storage paths
- `read_session()`: Parse recorder segments into `CanonicalSession`
- `write_session()`: Write `CanonicalSession` into FrankenTerm's native recorder format
- `resume_command()`: Return a FrankenTerm launch command

**2. CanonicalSession as FrankenTerm's Session Exchange Format**

FrankenTerm's `restore_layout.rs`, `restore_scrollback.rs`, and `restore_process.rs` modules handle session persistence. The `CanonicalSession` IR could serve as an interchange format:
- FrankenTerm could read `CanonicalSession` data to understand what commands/agents were running in each pane
- Agent conversation history could be correlated with terminal scrollback captured by the flight recorder

**3. Provider Detection for Agent-Aware Terminal Features**

FrankenTerm's `pane_tiers.rs` performs activity-based polling. casr's provider detection (`ProviderRegistry::detect_all()`) could inform FrankenTerm about which AI coding agents are installed, enabling:
- Automatic detection of agent panes
- Provider-aware session tagging
- Agent-specific keybindings or status bar indicators

**4. Session Resume Command Generation**

casr generates ready-to-paste resume commands via `Provider::resume_command()`. FrankenTerm could use this to offer one-click session resumption in a different provider from a terminal pane's context menu.

### Components Extractable for Reuse

**`model.rs` helpers**: `flatten_content()`, `parse_timestamp()`, `normalize_role()` are provider-agnostic utilities that could be used in FrankenTerm's agent detection and session annotation logic.

**`atomic_write()`**: The temp-then-rename write pattern in `pipeline.rs` could be adopted for FrankenTerm's own file persistence (recorder segments, config files) to prevent data loss from crashes.

**Provider storage path resolution**: Each provider's `home_dir()` / `session_roots()` implementations encode the exact filesystem layout of 14 AI coding agents. FrankenTerm could reuse this knowledge for agent detection and session discovery without re-implementing the reverse engineering.

**`SourceHint::parse()`**: The heuristic for distinguishing path vs alias from user input could be reused in FrankenTerm's CLI argument handling.

### How FrankenTerm's Flight Recorder Could Be a Provider

The flight recorder stores data in segments with the following structure (from `recorder_retention.rs`):
- Active segments -> Sealed -> Archived -> Purged lifecycle
- Each segment contains timestamped terminal events (output, input, resize)
- Audit trail (`recorder_audit.rs`) provides tamper-evident hash chains

A `FrankenTermProvider` would:
1. Scan recorder storage directories for session segments
2. Parse segment data to extract agent conversations (user commands + terminal output)
3. Map terminal events to `CanonicalMessage` entries (user input -> `MessageRole::User`, agent output -> `MessageRole::Assistant`)
4. Preserve terminal-specific metadata (pane dimensions, shell, cwd) in `extra` and `metadata` fields
5. For writing: create recorder segments from incoming `CanonicalSession` data to seed a new FrankenTerm session with context

### Integration Architecture Options

**Option A: Embed casr as a library dependency**
- Add `casr = { path = "/dp/cross_agent_session_resumer" }` to FrankenTerm's `Cargo.toml`
- Use `casr::providers::*` and `casr::model::*` directly
- Risk: pulls in `rusqlite` (bundled SQLite) which adds ~1.5MB; conflicts with FrankenTerm's `frankensqlite` may arise
- Note: casr uses Rust 2024 edition with nightly; FrankenTerm would need to be on nightly too

**Option B: Vendor-adapt relevant types**
- Copy `model.rs` types into `frankenterm-core` (following casr's own CASS independence pattern)
- Implement FrankenTerm-specific providers without pulling the full casr dependency
- Most sustainable for decoupled release cycles

**Option C: IPC via JSON**
- Shell out to `casr --json` for session operations
- Parse JSON output
- Lowest coupling but adds process spawn overhead

---

## R8: Upstream Tweak Recommendations

### 1. Add a Library-First Provider Registration API

Currently `ProviderRegistry::default_registry()` hardcodes all 14 providers. An `add_provider(Box<dyn Provider>)` method would allow external crates (like FrankenTerm) to register custom providers without forking:

```rust
impl ProviderRegistry {
    pub fn with_custom_provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.providers.push(provider);
        self
    }
}
```

### 2. Expose `atomic_write()` as a Public API

`atomic_write()` is currently `pub` but only within the crate. Making it `pub` in `lib.rs` re-exports would allow external users to leverage the safe write pattern.

### 3. Add Feature Flags for Provider Subsets

The SQLite providers (Cursor, OpenCode) pull in `rusqlite` with `bundled` which compiles SQLite from C source. A feature flag like `sqlite-providers` would let consumers opt out:

```toml
[features]
default = ["all-providers"]
all-providers = ["sqlite-providers", "markdown-providers"]
sqlite-providers = ["rusqlite"]
```

### 4. Support Stable Rust / Lower MSRV

casr uses Rust 2024 edition (nightly-only) primarily for let-chains and `if let` guards. These features are stabilizing. Once stable, pinning to a stable MSRV would broaden adoption.

### 5. Add `CanonicalSession` Builder / Factory

A builder pattern for `CanonicalSession` would make programmatic construction less verbose for integration consumers:

```rust
CanonicalSession::builder()
    .provider_slug("frankenterm")
    .workspace("/data/projects/foo")
    .message(MessageRole::User, "Fix the bug")
    .message(MessageRole::Assistant, "I found the issue...")
    .build()
```

### 6. Abstract the Session Storage Backend

The providers currently hardcode `dirs`-based filesystem paths. An abstraction layer for storage resolution would allow FrankenTerm to provide session data from its own storage backend (e.g., flight recorder segments) without filesystem tricks.

### 7. Timestamp Normalization Should Be Configurable

`parse_timestamp()` uses a hardcoded 100-billion threshold to distinguish seconds from milliseconds. Exposing this as a parameter (or offering explicit `parse_timestamp_seconds()` / `parse_timestamp_millis()`) would prevent ambiguity with FrankenTerm's own timestamp conventions.

### 8. Add a `Provider::capabilities()` Method

A method returning supported operations (`CanRead`, `CanWrite`, `CanResume`, `CanListSessions`) would let consumers dynamically query what a provider supports rather than trial-and-error:

```rust
fn capabilities(&self) -> ProviderCapabilities {
    ProviderCapabilities { read: true, write: true, resume: true, list: true }
}
```

### 9. Validate Session ID Format Per Provider

Currently `owns_session()` does ad-hoc ID matching. A `fn validate_session_id(&self, id: &str) -> bool` method would let consumers pre-validate IDs before attempting resolution.

### 10. Document the CASS Independence Pattern for External Adopters

The CASS independence policy (`docs/cass-independence-policy.md`) is well-documented internally. The same "vendor-adapt, don't depend" pattern should be recommended for downstream consumers like FrankenTerm, with explicit guidance on which types are stable vs internal.

---

*Analysis complete.*
