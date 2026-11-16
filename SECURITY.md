# Security Policy

`ft` / FrankenTerm is a swarm-native terminal platform that observes,
captures, and acts on AI agent terminal sessions. This document
describes how to report suspected vulnerabilities, the current trust
model, and the audit trail of shipped security fixes.

## Reporting a vulnerability

**Please do not open a public GitHub issue with vulnerability
details.**

Preferred reporting channel:

1. **GitHub Security Advisories** — open a private advisory under
   the repository's Security tab (`Report a vulnerability` button).
   This routes to the project maintainer without exposing details
   publicly.

   Private vulnerability reporting is enabled on a best-effort
   basis. If the `Report a vulnerability` button is not visible on
   this repo at the time you need it, open a *minimal* public issue
   titled `[ft-security] request private channel` — **without any
   vulnerability details** — and the maintainer will re-enable the
   private channel and follow up. The maintainer's GitHub profile
   does not currently publish a public email; do not send writeups
   to guessed addresses.

When reporting, please include:

- the shortest reproducer you can share (config snippet, input file,
  command sequence),
- the `ft --version` output,
- the affected surface (`ft watch`, MCP tool, `ft restart`, mux-server
  daemon, etc.),
- whether the report involves attacker-controlled input that crosses
  a trust boundary.

Expected response SLO:

- **Acknowledgement**: within 7 days of a private advisory.
- **Triage + severity classification**: within 14 days.
- **Fix timeline**: MEDIUM and above — 30 days from confirmed repro;
  CRITICAL — best-effort hotfix.

Out of scope for this report channel:

- Findings that require pre-existing DB write access or host
  compromise (the trust model below explicitly treats those as
  attacker-equivalent). Report them as normal bugs.
- DoS on unsupported platforms (targets are macOS + Linux).
- Issues in upstream WezTerm-heritage code that live in `frankenterm/`
  — those are better reported upstream.

## Current threat model

`ft` runs as the invoking user with no privilege separation. The
trust boundaries, in order of shrinking attacker capability:

- **Host compromise** (attacker has arbitrary code execution as the
  user) is out of scope — anything reachable from that position is
  already game over.
- **DB write access** — an attacker who can write the `.ft/` SQLite
  files can forge checkpoints, tamper mux pane state, or poison
  stored segments. Mitigations are defense-in-depth (CR/LF
  sanitization at the restore boundary, state-hash witness on
  checkpoint inserts) rather than integrity enforcement.
- **Repo-clone attacker** — a hostile repository can drop
  `.ft/patterns/*.yaml` pattern packs or `.ft/mission/*.json`
  mission contracts that get loaded by `ft watch` / `ft robot`
  surfaces. This is a real, attacker-reachable threat class;
  `ft-xv561` (ReDoS) and `ft-05hfm` (size caps) close concrete
  instances.
- **MCP client** — the stdio MCP transport inherits OS uid/gid and
  has no in-band auth. Any process able to speak MCP to `ft mcp
  serve` is trusted to the same level as the invoking user, but
  its *inputs* are still validated (workspace containment, size
  caps, approval gating on mutating tools).
- **Pane output** — captured terminal bytes are redacted on READ
  surfaces only (`get-text`, `search`, MCP responses). The on-disk
  SQLite store contains raw bytes; DB file permissions are the
  containment boundary.
- **Distributed mode peers** (feature-gated) use constant-time token
  comparison and TLS with WebPKI client/server verifiers. `rustls`
  is the only TLS stack; `danger_accept_invalid_certs`-style opt-outs
  are forbidden and unused.

## Fix history

| Date | Bead | Commit | Severity | Summary |
|------|------|--------|----------|---------|
| 2026-04-20 | ft-security-mcp-path-traversal | `30988e53` | CRITICAL | MCP `contract_file` / `mission_file` workspace containment. `resolve_workspace_scoped_path` rejects `..` components, joins relative paths to the workspace root, and canonicalizes the nearest existing ancestor before a `starts_with(workspace_root.canonicalize())` check. Blocks arbitrary file R/W under the watcher UID via hostile path args in `wa.tx_plan`, `wa.tx_run`, `wa.tx_rollback`, `wa.tx_show`, and the mission_* tools. |
| 2026-04-20 | ft-xv561 | `070117a0` | HIGH | Pattern-engine ReDoS cap. `compile_rule_regex` routes every rule compile through `RegexBuilder::new(raw).backtrack_limit(10M)`. Blocks hostile `.ft/patterns/*.{yaml,json,toml}` payloads from exhausting CPU via catastrophic-backtracking patterns like `(a+)+b`. |
| 2026-04-20 | ft-kegvt | `969b9e02` | MEDIUM | Restored-command CR/LF/ESC/C0 sanitizer. `sanitize_restored_command` rejects `\r`, `\n`, `\x1b` (ESC), and other C0 control bytes (except TAB) before `launch_agent_{legacy,cx}` sends a restored command to the pane shell. Blocks shell-line-injection via tampered `mux_pane_state.command` rows. |
| 2026-04-20 | ft-ybtyg | `598a7c13` | MEDIUM | Session checkpoint integrity witness + docs alignment. Replaced the hardcoded `'restore'` literal in `session_checkpoints.state_hash` with a real `compute_restore_state_hash(session_id, pane_id_map, now_ms)` — domain-separated SipHash-24. Docstring at `snapshot_engine.rs:14` and schema comment at `storage.rs:609` realigned from misleading "BLAKE3" to accurate "SipHash-24 (not cryptographic integrity)". |
| 2026-04-21 | ft-lokam | `e790f481` | LOW | Diag bundle `cwd` / path privacy scrubber. `gather_environment` routes `std::env::current_dir()` through `scrub_path_to_basename` by default; `DiagnosticOptions.include_full_cwd: bool` (default `false`) gates the absolute-path path. Blocks `/Users/<username>/project-name/` leaks in shared diag bundles. |
| 2026-04-21 | ft-gqbpk | `0f38972b`, `905cfc3d` | LOW | `frankenterm-mux-server` SIGTERM/SIGINT graceful shutdown. Async-signal-safe handler sets a `static AtomicBool`; the executor loop polls it between ticks. Clean shutdown triggers `Mux::shutdown()` and lets `main()` reach the existing `wezterm_blob_leases::clear_storage()` path. Fix also closes a startup SIGTERM cleanup race. |
| 2026-04-21 | ft-05hfm | `e8397747`, `8e57c8e0` | LOW/MEDIUM | Size caps on attacker-reachable buffers. `MAX_PACK_BYTES = 16 MiB` stat-check in `load_pack_from_file` before the deserializer sees anything; `MAX_SEND_TEXT_BYTES = 4 MiB` length-check in `WaSendTool::call` before any runtime setup. Blocks hostile-pack OOM and runaway `wa.send` payloads. |

Regression-guard tests landed in `0aa93816` (victory-lap review): symlink-escape rejection for MCP containment (relative + absolute variants, Unix-only); ReDoS-budget headroom proof against a 100 KiB realistic input so a future cap tightening can't silently break legitimate rules.

## What this document is not

- A CVE index. `ft` has no CVE pipeline today; fixes are tracked by
  bead + commit.
- A comprehensive threat model. The "Current threat model" section
  is intentionally short and operational; a deeper model is tracked
  separately in `docs/` and the `ft-xbnl0` finish-line program.
- A signing / provenance statement. Release artifacts are not yet
  signed; that work is tracked outside this document.
