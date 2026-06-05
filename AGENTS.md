# AGENTS.md — frankenterm

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 0.5: NEVER REMOVE crates/frankenterm-core

**THE crates/frankenterm-core CRATE IS A PERMANENT PART OF THIS WORKSPACE. REMOVING IT IS FORBIDDEN.**

This rule exists because agents have REPEATEDLY deleted this entire crate (860+ files, 624K+ lines of code) under the guise of "refactoring" or "consolidation." It has happened at least 3 times and caused enormous damage each time.

**ABSOLUTE PROHIBITIONS:**
- Do NOT delete, remove, or "consolidate" `crates/frankenterm-core/`
- Do NOT stage deletions of files in `crates/frankenterm-core/`
- Do NOT create commits that remove files from `crates/frankenterm-core/`
- Do NOT propose, plan, or suggest removing this crate for any reason
- The words "remove frankenterm-core", "consolidate into main crate", or "crate removal" should NEVER appear in any commit message

**If you see frankenterm-core files missing from disk but present in git:**
- This is the EXACT bug that keeps happening — an agent on another machine deleted them
- Do NOT commit the deletions. Run `git checkout -- crates/frankenterm-core/` to restore them
- Report the issue to the user

**This crate is not optional. It is not redundant. It is not "extracted." It IS the core.**

---

## AGENT MAIL (am) PROCESS PROTECTION — DO NOT TOUCH

**NEVER run any of these commands:**
- `am service restart` / `am service stop`
- `am doctor fix` / `am doctor repair` / `am doctor reconstruct`
- `kill` targeting any `am`, `am serve-http`, or `mcp-agent-mail` process

The `am serve-http` process is a **shared singleton** that all agents depend on. Restarting or killing it disrupts every other agent. Multiple agents running `am service restart` create a **restart cascade** that makes the service permanently unavailable.

**If `am` commands fail or the API is unreachable:** retry once after a few seconds, then proceed with your work WITHOUT agent-mail. Do NOT attempt to diagnose, repair, or restart the service. For coordination context while mail is red, run `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` and use the Beads/git snapshot as the handoff surface until Agent Mail recovers.

---

## macOS Debugging: Prefer LLDB for Native FrankenTerm Processes

On Apple Silicon macOS, Homebrew `gdb` may install successfully, but GNU GDB is
not a reliable local debugger for native `aarch64-darwin` FrankenTerm processes.
Codesigning with `com.apple.security.cs.debugger` is required for Mach task-port
access in cases where GDB can attach, but it does not fix the common Apple
Silicon failure mode where GDB reports `Don't know how to attach` or `Don't know
how to run`.

For local live-process diagnosis on this Mac, use Apple-native tools first:
- `lldb -b -p <pid> -o 'thread backtrace all' -o detach -o quit`
- `/usr/bin/sample <pid> 5 -file /tmp/frankenterm-<pid>.sample.txt`
- `spindump <pid>` for longer UI hangs or beach-ball investigations

Keep GNU `gdb` installed only as a remote-protocol frontend or for non-native
targets. Do not waste incident time trying to make Homebrew GDB replace LLDB for
native FrankenTerm GUI debugging on Apple Silicon.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## RULE NUMBER 1.5: FIX EVERY ISSUE YOU FIND — NO MATTER THE ORIGIN

When you are reviewing, auditing, or otherwise touching code in this
repo and you discover a problem — a bug, a panic-on-edge-case, a
resource leak, a race, a security gap, a silent-failure path, a
missing observability counter, an unbounded operation, a deprecated
API, an unhandled error, a clippy warning, anything that a careful
reviewer would call a defect — **you MUST optimally investigate,
diagnose, and fix it in the best, smartest, most correct way you
can.** You may not punt on the basis of:

- "Pre-existing — predates my work."
- "Not introduced by my commit."
- "Owned by another agent / bead."
- "Out of scope for this pass."
- "Tracked elsewhere in a bead."
- "Style only / hygiene only."
- "Will be addressed by some future refactor."
- "Probably never happens in practice."

None of these are valid reasons to leave a defect in place. If a fix
would meaningfully change the architecture or break a public API,
you should still do it — quality and correctness override
backwards-compatibility (we are pre-1.0; see "Backwards
Compatibility" below).

**The only acceptable reasons to defer a fix are:**

1. The fix is genuinely outside the scope of what you can do safely
   in the current session (e.g., it requires running a long
   migration, coordinating with a human stakeholder, or rewriting a
   subsystem larger than a single coherent commit). In that case,
   file a bead with a precise reproduction + proposed fix + the
   diagnosis you already did, then keep working.
2. Fixing it would produce demonstrably wrong behavior (e.g., the
   "fix" papers over a deeper root cause). In that case, document
   the deeper root cause and file the bead.

**Diagnose first, then fix.** Never patch a symptom without
understanding the root cause via first-principles analysis. A fix
that hides the symptom but leaves the cause is worse than no fix.

If you find ten issues, fix ten. If you find one hundred, fix one
hundred. The size of the backlog is not an excuse to leave
individual defects unaddressed.

---

## RULE NUMBER 2: ABSOLUTELY NO GIT WORKTREES

**GIT WORKTREES ARE STRICTLY FORBIDDEN IN THIS REPO. DO NOT USE THEM.**

1. **Never run:** `git worktree add`, `git worktree remove`, `git worktree prune`, or any related worktree command.
2. **No exceptions by convenience:** Do not create temporary directories, detached worktrees, or parallel checkout trees for agent work.
3. **Use branches in the main repo only:** All agent work must happen on normal branches in the primary checkout.
4. **If you discover existing worktrees:** stop and report them, then rescue useful commits back into normal branches.

---

## FIRST-TIME SETUP ON ANY MACHINE

Run this after cloning or on any machine where agents work on frankenterm:
```bash
bash scripts/install-hooks.sh
```
This installs a pre-commit guard that blocks mass deletions and any deletion of `crates/frankenterm-core/`.

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

## Weekly WezTerm Upstream Backport Workflow

We should periodically harvest fixes from upstream WezTerm, but **never** by
blindly pulling or merging upstream into this repo. FrankenTerm is an owned fork
with rebrand, asupersync/runtime policy, bundled defaults, and swarm-specific
architecture. Treat upstream as a read-only patch source and backport
deliberately.

**Hard rules:**
- Do NOT run `git pull` from `wez/wezterm` in this checkout.
- Do NOT merge upstream `main` into FrankenTerm.
- Do NOT create a git worktree or parallel checkout.
- Do NOT bulk-copy upstream directories over `frankenterm/` or
  `crates/frankenterm-gui/`.
- Do NOT accept upstream deletions without explicit written user permission.
- Preserve FrankenTerm naming, bundle IDs, icons, default Pragmasevka font,
  `runtime_async`/asupersync policy, and side-by-side WezTerm installation.

**Weekly process:**
1. Start only from a clean or fully understood worktree. If unrelated files are
   dirty, record them and do not mix them into the upstream-backport batch.
2. Find the upstream baseline from `frankenterm/PROVENANCE.json`
   (`divergence_point.subject` records the imported WezTerm commit).
3. Fetch upstream into a read-only tracking ref:
   ```bash
   git fetch https://github.com/wez/wezterm.git main:refs/remotes/wezterm-upstream/main
   ```
4. Build an inventory of upstream commits since the baseline, grouped by
   subsystem: `term`, `termwiz`, `window`, `config`, `mux`, `pty`, `font`,
   `ssh`, `codec`, GUI, and mux-server surfaces.
5. Prioritize security, crash, data-loss, terminal-correctness, macOS/windowing,
   PTY, SSH, and font fixes before cosmetic changes or features.
6. For each accepted upstream commit, inspect the patch with `git show` and
   manually port the smallest coherent slice into the renamed FrankenTerm paths.
   Avoid regex/script-based code rewrites.
7. Keep one upstream topic per FrankenTerm commit when practical. Include the
   upstream SHA(s) in the commit body as `Upstream-WezTerm: <sha>`.
8. If an upstream change conflicts with FrankenTerm architecture, do not force
   it in. Record it as skipped/deferred with the reason.
9. Validate with the narrowest relevant proof first, usually package-scoped for
   vendored crates (for example
   `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-mux-check cargo check -p mux --lib`
   and
   `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-mux-test cargo test -p mux --lib`
   for mux-only changes). Then run broader workspace checks when feasible.
   Report unrelated workspace or system-package blockers separately from the
   backport result.
10. Update provenance/backport notes at the end of the batch so the next weekly
    pass knows which upstream SHAs were accepted, skipped, or deferred.

Good backports should feel like FrankenTerm-native fixes with traceable upstream
provenance, not like a partial re-import of WezTerm.

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml workspace with `workspace = true` pattern
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]` via `[workspace.lints.rust]`)

### Async Runtime: asupersync

This project must use **asupersync** for async operations. The intended runtime model for the `frankenterm` CLI binary and `frankenterm-core` library is `Cx`-aware, structured, cancel-correct async built around asupersync.

**Policy:** direct `tokio` usage is forbidden. `runtime_async` (formerly `runtime_compat`) is the **canonical async API surface** of the project — asupersync wrappers (`Mutex`, `RwLock`, `Semaphore`, `mpsc`, `watch`, `broadcast`, `oneshot`) plus project-curated ergonomic helpers (`sleep_with_cx`, `timeout_with_cx`, `RuntimeBuilder`). Use it; don't reach for `asupersync::*` directly. The deprecated `runtime_compat` alias has been removed; all code must import via `crate::runtime_async`. See `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md` for the rationale.

**Enforcement (ft-i2eni.3 + ft-tf6g3.7):** the policy is enforced by
live type, source, test, dependency, and release-bundle gates.
(1) `runtime_proof::Sealed` — sealed trait makes `tokio::sync::*` types
fail to compile in `RuntimeProof`-bounded API surfaces (foundation
shipped under ft-i2eni.1; full adoption sweep closed under ft-3kv6e at 0
uncovered pub async fn). (2) `scripts/check_asupersync_test_only.sh` plus
`tests/wa_22x4r_no_tokio_test_in_supported_paths.rs` — CI and
cargo-test-time checks that no active `#[tokio::test]` attribute lands in
supported paths. (3) `crates/frankenterm-core/tests/common/asupersync_test.rs`
and the LabRuntime helpers — the supported async-test substrate. (4)
`dependency_eradication.rs` / `forbidden_dep_guards.rs` — source-level grep
guards for `use tokio::` and friends. (5) `deny.toml` `[bans]` rule + the
`Cargo-deny tokio ban (ft-i2eni.3)` CI step — fails the build if any
first-party `Cargo.toml` declares `tokio` as a direct dependency. The
classification and release-bundle evidence live at
`docs/tokio-test-classification.md` and
`docs/attestations/doctrine/tokio-eradication-status.json`.

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `asupersync` | Async runtime, structured concurrency, cancel-correct primitives |
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
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-workspace-check \
  cargo check --workspace --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-workspace-clippy \
  cargo clippy --workspace --all-targets -- -D warnings

# Verify formatting
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-workspace-fmt \
  cargo fmt --check
```

Remote proof must fail closed. If `rch` reports `[RCH] local`, `running
locally`, `no admissible workers`, `worker=null`, `local fallback`, or any other
path that did not reach a remote worker, stop the proof lane and mark the bead
blocked with the exact RCH reason. Do not count local Cargo output as proof.

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

`ft` is not defined by WezTerm integration. The intended architecture is an asupersync-native swarm runtime with `ft`-owned observability, policy, workflow, search, and robot/operator surfaces.

Current architecture reality:

- The core runtime model is `Cx`-aware, structured, cancel-correct async built around asupersync.
- `runtime_async` (formerly `runtime_compat`) is the canonical async API surface: a thin, project-owned wrapper over `asupersync` that exposes ~115 stable exports (sync primitives, channel modules, runtime lifecycle, time helpers). The wrapper is intentional and not going away. See `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md` for the importer audit (187 files / 1 622 references at rename time) and the rationale for keeping it.
- `ft` is a wezterm-fork mux runtime: the in-process mux session API is the `MuxInterface` trait (renamed from `WeztermInterface` in ft-zoxxq.1) and the <!--count:vendored_members-->47<!--/count--> vendored `frankenterm/<crate>/` workspace members are first-class — there is no plan to support a second mux backend. The "implementation boundary" framing was retired in ft-zoxxq.3; see `docs/proposals/ft-zoxxq-mux-boundary-truth.md` for the audit (7 803 LOC, 31 importers, 192 concrete-type refs, 0 trait-object consumers) that drove the decision. Verify the live count with `awk '/^members = \[/,/^]/' Cargo.toml | grep -c '^\s*"frankenterm/'` — `find -maxdepth 2 -name Cargo.toml` undercounts because `frankenterm/{config,dynamic}/derive` and `frankenterm/lua-api-crates/*` are nested deeper (ft-d3awp).
- Finish-line truth for support claims and verification lives in:
  - `docs/ft-xbnl0-verification-contract.md`
  - `docs/ft-xbnl0-3-6-supported-path-truth-sweep.md`
  - `docs/ft-xbnl0-4-6-completion-evidence.md`
  - `docs/ft-xbnl0-5-7-completion-evidence.md`
- The claim-to-artifact doorway is `docs/attestations/manifest.json`: each
  slot maps a claim category to a producing-bead artifact for the per-release
  bundle. Current target-class resource-cockpit status is separately pinned in
  `docs/attestations/proofs/resource-cockpit-target-class.json` under
  `ft-tf6g3.14`; its latest retained artifact is `skipped_not_proven`, so
  high-scale memory-envelope wording must wait for `ft-tf6g3.1` to sign a
  non-skipped target-class artifact.
- Any bead that creates or updates an attestation artifact must use
  `docs/release/attestation-checklist.md` and its producing-bead closing
  template before closing.

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
│   Current mux interop boundary (WezTerm-backed today)      │
└────────────────────────────────────────────────────────────┘
```

### Workspace Structure

```
frankenterm/
├── Cargo.toml                         # Workspace root
├── crates/
│   ├── frankenterm/                   # CLI binary (main.rs)
│   ├── frankenterm-core/             # Core library
│   │   └── src/
│   │       ├── runtime.rs            # Observation runtime orchestration
│   │       ├── runtime_async.rs     # Canonical async API surface (asupersync wrappers + Cx-aware helpers).
│   │       ├── ingest.rs             # Pane discovery + delta extraction
│   │       ├── patterns.rs           # Pattern detection engine
│   │       ├── events.rs             # Event bus and detection fanout
│   │       ├── workflows/            # Workflow modules (engine/runner/lock/handlers/traits)
│   │       ├── policy.rs             # Safety/access control
│   │       ├── storage.rs            # SQLite + FTS5
│   │       └── wezterm.rs            # Current live mux/pane interoperability adapter
│   │
│   │   # ── ft-y0loj.* sub-crates carved out of frankenterm-core (2026-04-25/27) ──
│   │   # <!--count:core_subcrates-->19<!--/count--> sub-crates extracted (ft-hdvvo + post-hdvvo work). For the live
│   │   # count and module totals, run: ls -d crates/frankenterm-core-* | wc -l
│   │   # and: find crates/frankenterm-core/src -maxdepth 1 -name '*.rs' | wc -l
│   │   # (currently <!--count:core_top_level_modules-->512<!--/count--> top-level modules at last stamp).
│   │   # Hand-edited stat figures here drift fast (ft-d3awp / ft-1b0rn /
│   │   # ft-f1vcd); the commands above are the source of truth.
│   │   # Type-only leaves have zero first-party deps; cluster sub-crates depend on
│   │   # frankenterm-core. No core → sub-crate edges (extraction is one-way).
│   │   # See docs/proposals/ft-l3tfo-cold-build-measurements.md for the cold-build ADR
│   │   # and docs/proposals/ft-t2d70-mcp-connector-extraction-feasibility.md for the
│   │   # tier-2 PARK ADR (mcp/connector cycle blockers).
│   ├── frankenterm-core-tantivy/          # Lexical search stack (ft-y0loj.1, cluster, ~16k LOC)
│   ├── frankenterm-core-ars/              # ARS subsystem 16 modules (ft-y0loj.2, cluster, ~14k LOC)
│   ├── frankenterm-core-fleet/            # fleet_dashboard (ft-y0loj.3 PARTIAL — rest blocked on cycles)
│   ├── frankenterm-core-replay/           # Replay subsystem 25 modules (ft-y0loj.4, cluster, ~25k LOC)
│   ├── frankenterm-core-resource-types/   # backpressure + memory-tier types (ft-usvnt, leaf, 2.3k LOC)
│   ├── frankenterm-core-error-types/      # WA-XXXX error code catalog (ft-g6sa8, leaf, 2.1k LOC)
│   ├── frankenterm-core-config-types/     # tuning_config types (ft-otfxs, leaf, 1.4k LOC)
│   ├── frankenterm-core-policy-types/     # policy audit/compliance/metrics/quarantine (ft-0pykm, leaf, 4.3k LOC)
│   ├── frankenterm-core-replay-types/     # decision graph + recorder metadata (ft-j1qjt.1, leaf, ~1.1k LOC)
│   ├── frankenterm-core-telemetry-types/  # ewma, histograms, sketches, snapshots (ft-yf2am, leaf, 4.9k LOC)
│   ├── frankenterm-core-cass-types/       # cass schema/error/types (ft-8cg6y, leaf, ~870 LOC)
│   ├── frankenterm-core-caut-types/       # caut envelope/error types (ft-2z15d, leaf, ~458 LOC)
│   ├── frankenterm-core-connector-types/  # connector telemetry types (ft-dfd16, leaf, ~273 LOC)
│   │
│   ├── frankenterm-gui/              # GUI binary crate
│   ├── frankenterm-mux-server/       # Headless mux server binary crate
│   ├── frankenterm-mux-server-impl/  # Shared mux-server implementation
│   └── frankenterm-alloc/            # Allocator/telemetry support crate
├── frankenterm/                       # In-tree FrankenTerm crates (ex-WezTerm; for live count run: find frankenterm -maxdepth 2 -name Cargo.toml | wc -l)
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
├── fixtures/                          # Test fixtures
└── scratch/                           # Throwaway Rust reproductions — NOT part of the workspace build
                                       # See scratch/README.md. Ad-hoc repo-root scratch files
                                       # (test_*.rs, ubs_*.txt, storage.sqlite3*) are .gitignore'd;
                                       # move to scratch/ when worth keeping.
```

### Current Module Map (Code-Grounded)

| Surface | Primary Location | Responsibility |
|---------|------------------|----------------|
| CLI command routing | `crates/frankenterm/src/main.rs` | Parses `Commands`/`RobotCommands` and dispatches watch/robot/workflow/mcp flows |
| Runtime orchestration | `crates/frankenterm-core/src/runtime.rs` | Discovery, capture, persistence, maintenance task graph |
| Async API surface | `crates/frankenterm-core/src/runtime_async.rs` | Canonical async API: asupersync wrappers (`Mutex`, `RwLock`, `Semaphore`, `mpsc`, `watch`, `broadcast`, `oneshot`) + Cx-aware helpers (`sleep_with_cx`, `timeout_with_cx`, `RuntimeBuilder`). Renamed from `runtime_compat` under ft-g43fq; the deprecated alias has been removed. See `docs/proposals/ft-7iof6-runtime-compat-canonical-surface.md`. |
| Ingest and deltas | `crates/frankenterm-core/src/ingest.rs` | Pane discovery, overlap matching, explicit gap semantics |
| Persistence and search | `crates/frankenterm-core/src/storage.rs` + `src/search/` | SQLite schema/migrations, FTS5, lexical/semantic/hybrid query paths |
| Pattern detection | `crates/frankenterm-core/src/patterns.rs` | Rule packs, anchor/regex evaluation, dedupe context |
| Event fanout | `crates/frankenterm-core/src/events.rs` | Bounded broadcast bus + typed runtime events |
| Workflow runtime | `crates/frankenterm-core/src/workflows/` | Engine/runner/lock + workflow traits/handlers |
| Policy gates | `crates/frankenterm-core/src/policy.rs` | Authorize/deny/require-approval decisions and rate limiting. Re-exports `Redactor` as a compat shim after ft-siwlu extraction. |
| Secret redaction | `crates/frankenterm-core/src/redactor.rs` | `Redactor` type + rules, extracted from `policy.rs` under ft-siwlu. Applied at every outbound pane-content read path (see `docs/security/read-path-redaction-matrix.md`). |
| Policy-denial audit | `crates/frankenterm-core/src/storage.rs` (schema v24+) | `policy_denied_audit` table + `PolicyDeniedAuditRecord` persist every Deny / RequireApproval from the MCP gate helpers. Wiring in `mcp_tools.rs::persist_mcp_policy_denial_async`; matrix at `docs/security/policy-denial-audit-wiring-matrix.md`. |
| Robot/MCP schemas | `crates/frankenterm-core/src/robot_types.rs` + `src/mcp*.rs` | Machine-facing envelopes and MCP tool/resource contracts |
| Mux session API | `crates/frankenterm-core/src/wezterm.rs` (+ `mux_client.rs` strangler-fig anchor) | `MuxInterface` trait (formerly `WeztermInterface`, alias preserved) for discovery, read/write, and pane ops; concrete types being incrementally relocated into `mux_client.rs` per ft-zoxxq.2 |

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
  "data": [
    {"pane_id": 0, "title": "claude-code", "domain": "local", "cwd": "/project"}
  ]
}
```

`ft robot state` serializes `data` as a bare `PaneState` array. The
`--include-text` variant wraps the array in an object instead:

```json
{
  "ok": true,
  "data": {
    "panes": [
      {"pane_id": 0, "title": "claude-code", "domain": "local", "cwd": "/project"}
    ],
    "tail_lines": 200,
    "escapes_included": false,
    "pane_text": { "0": "…tail…" }
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
ft robot wait-for 0 "codex.usage.reached" --timeout-secs 3600

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

#### Agent Inventory (feature-gated)

The `ft robot agents` family (`list`, `show`, `configure`, `configure --dry-run`)
is gated behind the `agent-detection` Cargo feature. The dispatch in
`crates/frankenterm/src/main.rs:21715` short-circuits via
`agent_correlator::filesystem_detection_available()`, which is
`cfg!(feature = "agent-detection")`.

- The default feature set **includes** `agent-detection`, so normal
  `cargo build` / release binaries have the full agent surface.
- A `cargo build --no-default-features` binary silently returns
  `robot.feature_not_available` for every call in this family. There is
  no runtime flag to re-enable it; you must rebuild.
- If you need agent inventory on a trimmed build, enable it explicitly:
  `cargo build --no-default-features --features agent-detection`.

Calls gated out by the feature return the FT-MCP-style envelope with
`ok: false`, `error_code: "robot.feature_not_available"`, and a hint
pointing back at this section.

### Graduated Robot NTM-Gap Families

The original NTM-gap family fallback has been retired for the checkpoint,
context, work, and live fleet CLI shapes. `ft robot fleet status` and
`ft robot fleet agents` use native agent-inventory/work-queue read paths.
`ft robot fleet scale` and `ft robot fleet rebalance` also use native plan
builders: dry-run requests return side-effect-free plan receipts, and
non-dry-run requests persist durable mutation receipts with idempotent replay.
Failed or denied mutations return typed `robot.fleet.*` envelopes rather than
falling back to the retired `robot.not_implemented` path.

`ft robot checkpoint` shipped a native snapshot/session adapter under
ft-bs9uh.2. `save`, `list`, `show`, and `delete` use the existing
session checkpoint tables. `rollback --dry-run` returns the restore plan;
non-dry-run rollback is approval-blocked until the robot policy gate lands.

`ft robot context` shipped under ft-bs9uh.3 and is no longer in this table:
`status`, `rotate`, and `history` use the native SQLite `pane_contexts` and
`context_rotations` registry. `rotate` records a durable receipt with optional
`--idempotency-key` replay semantics; the native registry stores metadata and
does not persist raw conversation content.

`ft robot work` shipped a native SQLite `work_claims` queue under ft-bs9uh.4:
`claim`, `release`, `complete`, `list`, `ready`, and `assign` no longer route
through the NTM-gap fallback.

`ft robot profile` shipped under ft-b0g7g and is no longer in this table:
read paths (`List` / `Show` / `Validate`) and dry-run `Apply` route through
`crates/frankenterm-core/src/robot_profile_handler.rs`. Only non-dry-run
`Apply` (which requires daemon-side pane spawning) still returns a typed
`robot.profile.spawn_failed` envelope, tracked under ft-b0g7g.cont.apply_spawn.

See README.md's supported-surface table for the user-facing status. The epic
tracking the remaining fleet mutation implementation is `ft-bs9uh` (Robot
NTM-gap implementation).

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
- `ft snapshot restore` and `ft restart` are wired. Use `--layout-only` to skip scrollback replay, and use `ft watch` when you want restore-on-startup behavior after an unclean shutdown.
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
packs = ["builtin:core"]

[workflows]
enabled = ["handle_compaction"]
max_concurrent = 3

[safety]
require_prompt_active = true
block_alt_screen = true
```

Operator-tunable runtime constants live under `[tuning]` sections such as `[tuning.runtime]`, `[tuning.patterns]`, and `[tuning.search]`.
See `docs/tuning-reference.md` for the full `TuningConfig` reference: every key, default, unit, validation guard, and starting ranges for 10-pane, 50-pane, and 200+-pane fleets.

### Related Tools

| Tool | Relationship |
|------|--------------|
| `ntm` | Adjacent orchestration tooling; ft is the swarm-native terminal platform. See **Swarm Orchestration Playbook** below for empirically-validated dispatch rules |
| `slb` | Simultaneous Launch Button (may integrate with ft workflows) |
| `caam` | Account manager (provides auth for AI agents ft orchestrates) |

---

## Swarm Orchestration Playbook

These rules are empirically validated against multi-agent swarm sessions
(2026-04-27 was the calibration session). They are *not* obvious from
reading the `ntm`, `vibing-with-ntm`, or `cc-hooks` skills. Skipping
them costs ~30 minutes of rediscovery per new operator.

### Rule SO-1: Prefer `--robot-send` over `--robot-interrupt --interrupt-msg` for cooperative agents

`ntm --robot-interrupt --interrupt-msg "<text>"` can crash a codex pane
if the message text isn't parsed cleanly by the codex CLI's interrupt
handler — observed at tick #11 of the 2026-04-27 session, where the
interrupt-msg leaked to zsh as `Reply: command not found` and the
codex process exited to a bare zsh prompt.

```bash
# DO
ntm --robot-send -t SESSION:0.N "your message"
tmux send-keys -t SESSION:0.N Enter

# DON'T (for cooperative agents)
ntm --robot-interrupt --interrupt-msg "your message" -t SESSION:0.N
```

Recovery if a codex pane has fallen back to zsh: `tmux send-keys -t
SESSION:0.N "cod" Enter` (or `cc` for claude) and re-dispatch.

### Rule SO-2: Always send `tmux Enter` after `ntm --robot-send` (twice for codex, ~2s apart)

CC panes do *not* auto-submit on `--robot-send`; the message lands in
the cc input area and stays buffered. Codex panes also need an Enter,
and frequently need a *second* Enter ~2 seconds after the first — the
first Enter sometimes becomes a literal newline in the prompt; only
the second triggers submission.

```bash
ntm --robot-send -t S:0.N "message"
tmux send-keys -t S:0.N Enter
# For codex panes, add:
sleep 2 && tmux send-keys -t S:0.N Enter
```

### Rule SO-3: Codex idle-placeholder text is not stuck-pane evidence

Codex panes display rotating idle suggestions ("Find and fix a bug in
@", "Explain this codebase", "Summarize recent commits", "Use /skills
to list available...") when waiting for input. These are placeholder
hints, not signs the agent is stuck.

Real stuck-pane evidence requires *all* of:
- Identical TOOL-OUTPUT lines across consecutive ticks.
- Zero new commits attributable to the pane.
- No `br update` activity from the pane's assignee.

### Rule SO-4: CC convergence language is explicit; codex convergence is silent

CC panes will emit literal "converged" or "converged." replies when
prompted with a single-shot CONFIRM nudge. Codex panes typically do
*not* — they go quiet or emit a brief "Working" line and idle.

```text
Convergence threshold:
  - 2+ explicit cc "converged" replies, AND
  - remaining codex panes idle with no defects in last N ticks
```

Do not require unanimous explicit reply across pane types.

### Rule SO-5: `commits-1h ≤ 2` lags real convergence by ~45 min — tighten to `≤ 4`

The `commits-1h` window only ages out commits made well *before*
convergence; new convergence-burst commits keep the count high.
Pragmatic stop signal:

- 0 ready beads, AND
- ≥2 cc panes "converged", AND
- remaining in_progress beads have commit linkage in last 30 min.

Either tighten the threshold to `commits-1h ≤ 4` to match observed
reality, or rely on the per-pane convergence signal.

### Rule SO-6: Disk pressure is manageable with per-agent target dirs + completion cleanup

Each agent uses a unique `/tmp/ft-swarm-<slug>-target` (or
`/tmp/ft-<slug>-target`) so cargo locks don't contend. The 2026-04-27
session peaked at 96% disk with 7 active 98 GB targets and self-cleaned
to 33 GB by session end.

Operator dispatch nudges should always include target-dir lifecycle reporting
once the assigned bead is done — and especially once disk crosses 95%. Ask the
agent to report `CARGO_TARGET_DIR`, whether it is kept for incremental reuse,
approximate size if known, and whether cleanup is requested. Do not tell agents
to delete `/tmp/ft-<slug>-target/release` directly unless the user has given
explicit written deletion authorization for that exact path. Prefer
`scripts/clean-stale-targets.sh --inventory --threshold-hours 12` and a
reviewed dry-run before any cleanup request.

### Rule SO-7: Use `ntm --robot-send` for repeated nudges; `ntm send` is CASS-deduped

`ntm send` runs through CASS dedup, which blocks repeat sends unless
`--no-cass-check` is passed. For orchestrator nudges that are
intentionally repetitive (e.g., every-4-minute health pings),
`--robot-send` is non-interactive and bypasses the dedup prompt.

```bash
# Orchestrator periodic nudge — preferred
ntm --robot-send -t S:0.N "tick-check: still working?"

# Interactive operator message — also fine
ntm send S:0.N "tick-check: still working?"
```

### Rule SO-8: Long-running in_progress beads — broadcast first, force-release only on silence

Beads in_progress for >1h with no commit linkage in the last 30 min
should *not* be auto-released at a 2h cutoff. Broadcast a status-check
nudge first:

```text
"<assignee>: ft-XXXX in_progress for 1h+ with no commits.
 Commit and close, OR `br update --status open ft-XXXX --assignee=''`."
```

Only force-release after no response. The 2026-04-27 cyanbolt pane
voluntarily released ft-1memj.28 in response to such a broadcast —
preserving agent autonomy is what made it work.

### Cross-references

- `docs/operator-runbook.md` — tick-by-tick procedural how-to that
  applies these discrete rules.
- `vibing-with-ntm` skill — operator-tick playbook with concrete
  command sequences.
- `ntm` skill — primitive reference for `--robot-send` /
  `--robot-interrupt` / `send`.
- `scripts/swarm-tick.sh` and `scripts/clean-stale-targets.sh` —
  the operator helpers these rules drive.

---

## Testing

### Testing Policy

Every component crate includes inline `#[cfg(test)]` unit tests alongside the implementation. Tests must cover:
- Happy path
- Edge cases (empty input, max values, boundary conditions)
- Error conditions

Cross-component integration tests live in the workspace `tests/` directory.

### Methodology playbooks

For formal-methods and statistical-rigor work introduced by the
reality-check bridge plan, see:

- [`docs/methodology/proof-techniques.md`](docs/methodology/proof-techniques.md)
  — when to use Loom / TLA+ / Stateright / proptest / dylint /
  cargo-deny, with exemplar files in this repo.
- [`docs/methodology/statistics.md`](docs/methodology/statistics.md)
  — sequential testing, concentration-of-measure sample sizing
  (Hoeffding + Bernstein), conformal SLO bands, Mann-Whitney U /
  KS, with cross-links to `bench_stats::*` functions.

These playbooks are the canonical entry point — paste-and-fill from
them rather than rediscovering tooling questions.

For the per-substrate audit pattern catalog (pub-field bypass,
subprocess argv injection, missing DoS caps, attestation/release-
gate vacuous-pass, redactor pattern drift), see
[`docs/audit-checklist.md`](docs/audit-checklist.md).

### Unit Tests

```bash
# Run all tests across the workspace
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-workspace-test \
  cargo test --workspace

# Run with output
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-workspace-test-output \
  cargo test --workspace -- --nocapture

# Run tests for a specific crate
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-ft-test \
  cargo test -p frankenterm
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-core-test \
  cargo test -p frankenterm-core

# Run specific test by name pattern
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-pattern-test \
  cargo test pattern_matching

# Run tests with all features enabled
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-all-features-test \
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
  repoPath: "/data/projects/frankenterm-rch",
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
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-build cargo build --release
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-test cargo test
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-clippy cargo clippy
```

For proof lanes, a remote worker must actually be selected and reached. Output
that says `[RCH] local`, `running locally`, `worker=null`, `no admissible
workers`, or `local fallback` is blocker evidence only; it is never a successful
Rust proof.

Quick commands:
```bash
rch doctor                    # Health check
rch workers probe --all       # Test connectivity to all 8 workers
rch status                    # Overview of current state
rch queue                     # See active/waiting builds
```

### When rch is down: the exit-143 failure mode

The fails-open story is **aspirational**, not current. When `force_remote=true` is set in `~/.config/rch/config.toml` and the remote workers are unhealthy, the intercepted cargo subprocess receives **SIGTERM (exit 143)** with no diagnostic. Operators see `cargo test foo ... exit 143` and must treat that as a blocked remote proof lane, not as permission to run Cargo locally.

**Symptoms:**
- `cargo <anything>` exits with code 143 and no stderr explanation
- `rch doctor` shows workers down or timing out
- `rch workers probe --all` shows most/all workers unreachable

**Fail-closed procedure:**

```bash
rch doctor
rch workers probe --all
```

Record the failed Cargo command, RCH health output, selected-worker/admission
state, and exact reason code in the bead. Keep proof-required beads open or
blocked until a remote RCH run reaches Cargo/test execution and emits retained
proof artifacts. Static/read-only checks may support diagnosis while workers
are unavailable, but they do not replace remote Cargo proof.

Do not run local Cargo as the fallback for an exit-143 proof lane. If the human
operator explicitly requests an emergency local diagnostic, record it as
non-closeout context only; it is not proof for the bead, and the bead remains
open or blocked until RCH produces retained remote Cargo artifacts.

**Note for Codex/GPT-5.2:** Codex does not have the automatic PreToolUse hook, but you can (and should) still manually offload compute-intensive compilation commands using `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- <command>`. This avoids local resource contention when multiple agents are building simultaneously while keeping proof lanes fail-closed.

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
- **Agent Mail unavailable:** Retry once, then do not repair/restart/kill the shared service. Run `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` and continue with Beads-only coordination.

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

**Cycle-check truthfulness:** `bv --robot-triage` is a prioritization
surface, not the authoritative all-history cycle detector. On large Beads
graphs, `bv --robot-insights` may report `status.Cycles.state = "skipped"`
while triage still displays `graph.has_cycles = false`; do not treat that as
proof that the full dependency graph is acyclic. For dependency-editing or
idea-wizard refinement passes, run:

```bash
br dep cycles --json
```

If `br dep cycles --json` reports only closed/legacy `wa-*` cycles and no
active `ft-*` cycles, say that explicitly instead of claiming the whole graph
is cycle-free.

### Robot-Suggest Hygiene Checkpoint

Run `bv --robot-suggest` after large planning, reality-check, or idea-wizard
batches, and before closing a Beads graph-hygiene lane. Treat the output as an
advisory worksheet only. A high-confidence suggestion is not enough evidence to
mutate the graph.

Required classification workflow:

1. Capture the `generated_at`, `data_hash`, total suggestion counts, per-type
   counts, and high-confidence/actionable counts from `bv --robot-suggest`.
2. For each candidate dependency edge, inspect both sides with `br show --json`
   and use `br dep tree <id>` when hierarchy or historical closure could make
   the edge redundant.
3. Apply edges only when descriptions, acceptance criteria, comments, or
   artifact flow prove a real ordering relationship. Shared labels, shared
   keywords, or duplicate-looking acceptance text are not enough.
4. Classify every rejected or deferred live `ft-*` dependency suggestion as one
   of: `already_implied`, `status_mismatch`, `closed_historical_only`,
   `needs_human_or_domain_context`, or `pure_keyword_collision`.
5. Never run legacy `bd` `action_command` strings printed by old tooling in
   this repo. Recreate any accepted mutation with the current `br` command
   after verification.
6. Stop without mutating the graph when the evidence is ambiguous or when the
   change would make the ready queue less truthful.

Closeout proof footer for this checkpoint:

```text
bv --robot-suggest generated_at=<timestamp> data_hash=<hash>
suggestions: total=<n> missing_dependency=<n> potential_duplicate=<n> label_suggestion=<n> high_confidence=<n> actionable=<n>
br dep cycles --json: count=<n>; active_ft_cycles=<yes|no>
br sync --flush-only --json: errors=<n> success_rate=<float>
```

The first audit example is the `ft-uicx9` lane: `ft-uicx9.1` captured and
classified the worksheet, `ft-uicx9.2` verified high-confidence dependency
edges before declining mutations, and `ft-uicx9.3` documented repeated false
positive families.

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

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations (Claude Code, Codex, Cursor, Gemini, ChatGPT, etc.) so we can reuse solved problems.

**Rules:** Never run bare `cass` (TUI). Always use `--robot` or `--json`.

### Examples

```bash
cass health
cass search "async runtime" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json
cass capabilities --json
cass robot-docs guide
```

### Tips

- Use `--fields minimal` for lean output
- Filter by agent with `--agent`
- Use `--days N` to limit to recent history

stdout is data-only, stderr is diagnostics; exit code 0 means success.

Treat cass as a way to avoid re-solving problems other agents already handled.

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
