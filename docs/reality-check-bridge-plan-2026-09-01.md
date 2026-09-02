# FrankenTerm Reality-Check Bridge Plan — 2026-09-01

**Source:** `/reality-check-for-project` invocation 2026-09-01 (third full run). Read AGENTS.md (1,747 lines) and README.md (4,454 lines) in full, then audited HEAD `f3397177e` (workspace version `0.15.2-rc.1`) against the promises in those two documents. **Revised in place 2026-09-02** with the Phase 2 deep closure plan (§4) after the same-day execution recorded in §11.

**Predecessors (cross-link required by `docs/process/reality-check-discipline.md`):**
- `docs/reality-check-bridge-plan.md` — 2026-04-30 run, G1–G15, epics `ft-syqcz`/`ft-i2eni`/`ft-hac7w`/`ft-x0666`/`ft-t9a6q`/`ft-35yac` (closed at bead level).
- `docs/reality-check-bridge-plan-2026-05-12.md` — 2026-05-12 run, G16–G49, epic `ft-tf6g3` (75 closed / 5 blocked children).
- Peer threads that consumed the previous plans: `ft-e87u6` (attestation close-the-loop, open, no activity since 2026-06-15), `ft-7h5da` (2026-06-06 Dueling-Wizards W0–W12 program, 60 closed / 48 open-or-blocked), `ft-d0ez0` (end-to-end swarm proof, open since 2026-04-06).

**Trigger evidence (`scripts/check-reality-check-due.sh --json`, 2026-09-01):** `due: true`. Calendar 112 days since 2026-05-12 (threshold 90); minor version 0.7 → 0.15; open beads 940 (threshold 50); contract-doc churn 3,093 changed lines (threshold 50). Headline-claim growth did not trigger (56 vs baseline 62).

**Beads/BV evidence at run start:** 4,944 issues; 588 open / 230 in_progress / 117 blocked / 5 deferred / 4,004 closed. `bv --robot-triage`: open 940, actionable 315, blocked 625, in_progress 230, top pick `ft-e87u6`. `br dep cycles --json`: 0 cycles (active scope). Open beads by priority: P0 434, P1 239, P2 187, P3 80.

**Operating principle:** this file is revised in place through the ambition and refinement passes of this run. It never overwrites the two earlier plans.

---

## 1. Reality assessment — where we really are

FrankenTerm is an enormous, mostly real codebase (1.30 M lines in `frankenterm-core/src`, 83 workspace crates, 61.7 k test annotations, 11 TLA+ specs with a retained TLC run, 9 Loom suites, a Lean proof script) whose **individual subsystems are implemented and unit-tested, but whose core product loop has never been demonstrated end-to-end on real agents, and whose shipped install path is currently broken.** The 2026-06-06 duel program diagnosed this precisely ("the expensive 95 % is built, the cheap last 5 % of wiring is missing") and that diagnosis still holds three months later. Since June the project has added beads faster than it closes them and has turned most of its attention to a 660-child interactive-performance/product-convergence program while the first-run experience regressed.

**2026-09-02 delta.** One day of execution (§11) moved the picture in three ways. (1) The attach ramp now works for same-generation pairs on a dev build: discovery finds the GUI-published socket, version skew is a typed error, and a headless vendored mux plus `ft` from one build completes discover → attach → list → send → observe → detect on the maintainer's Mac. (2) That smoke exposed four defects that had nothing to do with wiring and everything to do with never having run the loop: the mux listener aborted on every connect, the client handshake poisoned itself on a benign broadcast, the flagship Codex usage-limit rule missed the real message casing, and the writer dropped a segment under ordinary intra-process contention. (3) Nothing in that list is a release artifact yet; the shipped v0.15.1 is still unsigned and un-activated. The gap register therefore grows (G71–G78) even as G51/G57/G61/G63/G64 shrink.

### The five questions

**1. What specifically IS working right now (code-verified, tests exist)?**
- Watcher runtime: discovery → 4 KB overlap delta → gap events → FTS5 persistence → pattern detection (Bloom + Aho-Corasick + fancy_regex, 94 rules across 4 agent families with 251 corpus fixtures) → event bus → workflow runner. `runtime.rs` wires native push events, BOCPD, backpressure tiers, fleet memory, and the recorder. The unclean-session detector runs at `ft watch` start (`main.rs:42598`; README under-claims this).
- Workflow reaction path: `StepResult::SendText` is executed through the policy-gated injector with `ActorKind::Workflow` (`workflows/runner.rs:2451-2470`); `--auto-handle` builds the runner (`main.rs:42847`). Handlers for compaction, usage limits, Claude/Gemini limits, auth-required, session start/end, process triage, cass search, swarm learning all exist and several emit real send steps.
- Tx engine: `execute_step_action` sends through the mux (`tx_execution.rs:2447-2518`); prepare phase uses `PolicyPrepareStepExecutor` wired to the real policy engine; TLA+ kill-switch spec model-checked (258 states, 0 violations).
- Robot Mode: 50 command families, zero live `robot.not_implemented` routes, honest fail-closed sites for snapshot restore / restart / checkpoint rollback. Verified-submit re-reads the pane after send. Prometheus `/metrics` server exists behind the (now documented) `metrics` feature.
- GUI: FrankenTerm.app 0.13.0 runs continuously on the maintainer's Mac with SSH-tunnelled remote mux domains. Windows, Linux, macOS CLI + app assets were published for v0.15.1.
- Session persistence is honestly scoped: every non-dry restore/rollback/restart fails closed exactly as the README says.
- **New since 2026-09-02 (dev build, not released):** ranked mux socket discovery incl. the GUI-published socket; typed `robot.mux_version_skew`; `ft tx run/rollback` resolve pane capabilities; standalone `ft web` tails storage into the event bus; `ft robot profile create`; a headless observe→detect smoke that passes on a real vendored mux.

**2. What is NOT working or not implemented?**
- **Install:** the v0.15.1 release manifest marks every artifact `"signed": false` and no `.minisig` asset exists, while `install.sh` exits 1 when minisign verification fails unless `--no-verify`. Even on success the process family is left `activation: pending` (no `ft` on PATH) because the cross-launcher lease transaction is not implemented (an idle-host `--activate` path landed 2026-09-01 but has no end-to-end test). The documented Quick Install cannot produce a working `ft` today.
- **CLI ↔ GUI attach on shipped binaries:** the 0.13.0 app and any HEAD CLI differ in codec generation; the only same-generation pair that has attached is a dev `frankenterm-mux-server` + dev `ft`. The GUI listener shares the accept-loop code that aborted before 647d87fd6, so **every HEAD-built GUI before that fix dies when a client connects**; no released app has the fix.
- **Live proof of the loop:** `ft-d0ez0` (P0, open since April) closed its 3/20/50-pane children on a fake WezTerm adapter; `ft-d0ez0.5` records that the 50-pane assertions were unconditional (false-green). The observe → detect → react → audit loop has been run once on a real mux with one shell pane (§11), never on live agents, never with `--auto-handle`, never with a retained signed artifact.
- **Dogfooding:** the only `ft.db` on this machine is the repo workspace one: 863 segments, 1 event, last capture **2026-02-14**, stale `watch.lock` from 2026-08-06.
- **Attestation moat:** only `docs/attestations/0.0.0-dev.json` (`method: unsigned`) exists; no bundle for v0.13/v0.14/v0.15.x. The static gates now live in `scripts/release-gates.sh`, but DSR does not run it yet.
- **Perf evidence:** `docs/perf/releases/` holds only a README; the swarm-capacity envelope is `blocked_target_class_not_proven`; the target-class cockpit is `skipped_not_proven`; the load rig emits synthetic metrics (`ft-7h5da.10.3.2`) and the cockpit script runs no bench lane (`ft-7h5da.10.4.3`).
- **Security items still open:** kill-switch SoftStop/HardStop tiers fail open for workflow/connector/file/exec actions (`ft-l59nq`, P1, fix committed 2026-06-15, never proven green) and the engine state is process-local (a fresh doctor run cannot see the watcher's live tiers).
- **Storage under contention:** the writer dropped a captured segment 350 ms after watcher start (deferred transaction read-then-write upgrade refused; fix in tree, unproven live) and the capture-side sequence counter never re-converges after a drop.
- **Test suite:** fresh core-lib baseline 2026-09-01: 31,256 pass / 4 fail (all four fixed the same day); one load-sensitive virtual-time test failed once on a loaded worker; RCH fleet drift (a worker vanished mid-session) makes proof lanes slow and brittle.
- **Process drift:** last upstream WezTerm backport commit is 2026-05-13 (weekly cadence promised); four more tracked orphan source files were found by the new guard (baselined, deletion needs authorization).

**3. What is blocking us?**
- The cross-launcher lifetime / PTY-guardian handoff transaction (activation on a live host) is designed but not built; the idle-host path exists but is untested end to end.
- Signing was never wired into the DSR release path, so verification-required installers reject DSR output; the fix is an operator-driven `dsr release` with signing on, not code.
- RCH admission for core-class proof lanes is scarce and the fleet is heterogeneous (hz2 disappeared 2026-09-02; hz3 sync timeouts); cold builds take 40–60 min per lane.
- Priority inflation was reset 2026-09-01 (P0 434 → 25) but the two July mega-epics still hold 663 children.

**4. If every open and in-progress bead were implemented, would the gap close?**
Closer than on 2026-09-01, because the entry-ramp defects now have beads (ft-xxfwy.1–.36). Still no: the closure conditions in §4 require retained, generation-bound artifacts (a signed release, an attach e2e against a shipped app, a live-agent loop proof, measured headline claims) that no existing bead produces on its own, and the July programs would qualify the product at scale without ever exercising the first-run path.

**5. Vision goals with zero bead coverage:** none after this revision (§7 maps every gap G50–G78 to a child of `ft-xxfwy`; V15/V28/V29 are honestly documented non-goals for this epic and are tracked elsewhere: `7xqz4.8`, `ft-azsnz`, README "not started").

---

## 2. Vision checklist

Status vocabulary: WORKING / PARTIAL / STUB / UNPROVEN / NOT_STARTED / REGRESSED / NO_BEAD. Statuses marked † changed on 2026-09-02 (dev build evidence only unless a commit hash is a release).

| # | Goal (README/AGENTS promise) | Status | Severity | Bead coverage | Evidence |
|---|---|---|---|---|---|
| V1 | `curl \| bash` installs a runnable `ft` (+ app on Apple Silicon) | REGRESSED | Critical | .1 .2 .3 .4 .30 | v0.15.1 manifest `signed:false`, 0 `.minisig`; `install.sh` exits 1; activation `pending` (idle-host `--activate` path landed, untested e2e) |
| V2 | `ft` attaches natively to the running FrankenTerm mux without external `wezterm` | PARTIAL † (WORKING on dev build vs headless mux; shipped pairs still skewed) | Critical | .5 .6 .7 .8 .33 .34 | §11: doctor `source: gui_published` vs the app; headless smoke green after 647d87fd6/0f00b7cf5 |
| V3 | Watch loop: discovery, delta, gaps, scheduling, push events, maintenance | WORKING (code), PARTIAL † (live: one pane, one detection) | Major | .9 .10 .32 | `scripts/smoke/headless-mux-observe.sh` PASS; segment drop + seq desync observed |
| V4 | Pattern packs for Codex / Claude Code / Gemini / runtime; lint; fixtures | PARTIAL † (flagship codex rule missed real casing until e903495ee) | Major | .27 | strings of codex 0.133 binary vs regex |
| V5 | Workflows auto-handle events with policy-gated sends | WORKING (code), UNPROVEN (live) | Major | .9 .10 .14 | `runner.rs:2451-2470`, `main.rs:42847`; smoke ran without `--auto-handle` |
| V6 | Policy gate: prompt-active, alt-screen, rate limits, approvals, audit | WORKING; practical gap | Major | .12 .13 .14 | CLI tx capability resolution landed (3ae637088 lineage); shell-integration setup missing; kill-switch state process-local |
| V7 | Robot Mode 50 families, fail-closed where unbuilt | WORKING † (+ `profile create`) | — | .28 | 5945f1127 |
| V8 | MCP server mirrors Robot Mode | WORKING | Minor | .25 (closeable) | orphan removed 917a68c4c; guard test |
| V9 | Web API + live SSE `/stream/events` | WORKING † (storage tail, dev) / UNPROVEN e2e | Major | .18 .19 | a950faf99, 744e52729 |
| V10 | Distributed mode | WORKING (tests) | — | .28 | `distributed_streaming_e2e.rs`; not re-verified live |
| V11 | Lexical / semantic / hybrid search | WORKING (feature-gated); Tantivy path UNPROVEN | Minor | .28 | `main.rs:9474-9478` |
| V12 | Mission + Tx prepare/commit/compensate on real panes | PARTIAL † (CLI capability gap closed in code) | Major | .12 .13 | `resolve_tx_contract_capabilities` at five sites; e2e pending |
| V13 | Operating envelope fail-closed admission | PARTIAL / UNPROVEN | Major | .20 | artifact `blocked_rch_no_verdict`; target-class `skipped_not_proven` |
| V14 | Fleet memory + 3-tier scrollback + backpressure at 200 panes | PARTIAL (synthetic only) | Major | .10 .20 | load rig synthetic; false-green 50-pane test |
| V15 | Session persistence (honestly scoped restore-unavailable) | WORKING as documented | — | `7xqz4.8` (outside epic) | fail-closed sites `main.rs:26547-26594` |
| V16 | Incident bundles with live collectors | UNPROVEN | Minor | .28 | not verified |
| V17 | Every headline claim → signed per-release attestation | REGRESSED | Critical | .15 .16 | only `0.0.0-dev.json` unsigned; gates in `scripts/release-gates.sh`, DSR not wired |
| V18 | Perf targets (<50 ms capture, <10 ms FTS, 200-pane ~200 MB) | PARTIAL (hedged) | Major | .20 | no `docs/perf/releases/*.json` |
| V19 | Formal methods (Loom, TLA+, explicit-state models, Lean) | WORKING (mostly) | Minor | .29 | TLC pass retained; Lean not gated; README wording fixed |
| V20 | Test suite green | PARTIAL † | Major | .17 | 31,256/4 → 4 fixed; retry test load-sensitive |
| V21 | Live observe→detect→react→audit proof at 3/20/50 panes | NOT PROVEN (tier-1 precursor on one shell pane †) | Critical | .9 .10 | smoke evidence; no agents, no auto-handle |
| V22 | README demo recordings | PARTIAL † (embeds removed; no recording) | Minor | .21 | `assets/demo*.gif` absent |
| V23 | Documentation accuracy | WORKING † (sweep landed) | Minor | .22 (closeable) | `stamp-readme-counts.sh --check` pass |
| V24 | Weekly WezTerm upstream backports | REGRESSED | Major | .23 | last `Upstream-WezTerm` commit 2026-05-13 |
| V25 | DSR-only releases, no GitHub Actions | WORKING † (workflows retired) | Minor | .16 | ab4bc7771 lineage |
| V26 | Atomic process family + PTY guardian handoff + activation | PARTIAL | Critical | .3 .4 | live-host activation unimplemented |
| V27 | Program health / bead graph as a truthful ready queue | PARTIAL † | Major | .24 | P0 25; 71 stale claims released; create>close trend not yet reversed |
| V28 | Windows support | PARTIAL | Minor | `ft-azsnz` (outside epic) | `ft-windows-amd64.zip` shipped |
| V29 | WASM extension runtime | NOT_STARTED (documented) | — | none (honest) | README honest |
| V30 | Maintainable codebase ("no tech debt") | PARTIAL | Major | .26 .31 | `main.rs` 135,372 lines; 4 tracked orphans |

---

## 3. Gap register (G50–G78)

Each gap carries the `proof_category:` line required by `docs/proof-taxonomy.json` (IDs 1–12 or the non-proof slugs substrate / process / infrastructure / coordination). Gaps G50–G70 are as recorded on 2026-09-01; G71–G78 were found on 2026-09-02 by actually running the loop.

### G50 — Latest release is not installable through the documented path — REGRESSED (Critical)
**Promise:** "The installer verifies the DSR-published checksum and minisign signature, then publishes `ft`…" (README Quick Install).
**Reality:** `frankenterm-v0.15.1-manifest.json` lists four artifacts with `"signed": false, "signature_file": ""`; the release has no `.minisig` asset; `install.sh` requires `minisign` and exits 1 on verification failure. `--no-verify` is the only way through, and even then the process family stays `activation: pending` until a transaction that (for live hosts) does not exist runs. `scripts/release/verify-release.sh` now exits 1 on v0.15.1; `install.sh --activate <gen> --idle-host-confirmed` exists without an e2e.
`proof_category: 10 (cryptographic-attestation) + process`

### G51 — `ft` cannot discover or attach to the running FrankenTerm.app — PARTIAL (Critical)
**Promise:** "Native vendored builds prefer the direct pooled mux protocol; the external `wezterm` CLI is a compatibility fallback."
**Reality 2026-09-02:** discovery and skew diagnostics are fixed on HEAD (§11) and a dev pair attaches to a headless mux; no shipped app/CLI pair can attach because the app is 0.13.0 and HEAD's listener fix is unreleased.
`proof_category: 4 (conformance-artifact) + 9 (differential-comparison)`

### G52 — Core loop never proven on live agents; prior proofs were false-green — NOT PROVEN (Critical)
**Reality:** `ft-d0ez0.1-.3` closed on a fake adapter; `ft-d0ez0.5` documents unconditional assertions. One real-mux, one-pane, one-detection smoke exists (§11) with no agents and no auto-handle.
`proof_category: 4 (conformance-artifact) + 5 (quantitative-attestation)`

### G53 — No dogfooding of the observe loop — process (Critical signal)
**Reality:** `scripts/dogfood-status.sh` reports `stale` (newest capture 4,793 h old); no watcher runs on the maintainer's machine.
`proof_category: process`

### G54 — Attestation graph has no real release bundle — REGRESSED (Critical for the stated moat)
**Reality:** `docs/attestations/0.0.0-dev.json` unsigned only; static gates consolidated into `scripts/release-gates.sh` (28 pass) but DSR's `required_checks` still run only check/clippy/test/format.
`proof_category: 10 (cryptographic-attestation)`

### G55 — Prompt-active evidence gap — PARTIAL (Major)
**Reality:** CLI tx sites resolve capabilities now; without shell integration every untrusted actor still gets `RequireApproval`; no `ft setup shell-integration`; the README tour's first send is not honest about that.
`proof_category: 4 (conformance-artifact) + 2 (property-test)`

### G56 — Kill-switch tiers fail open (security); engine state process-local — UNPROVEN fix (Major)
`proof_category: 2 (property-test) + 4 (conformance-artifact)`

### G57 — Web `/stream/events` publisher-less — WORKING in code (dev), UNPROVEN e2e (Major → Minor)
`proof_category: 4 (conformance-artifact)`

### G58 — Headline performance claims have no measured release artifacts — PARTIAL (Major)
`proof_category: 5 (quantitative-attestation)`

### G59 — Test-suite health — PARTIAL (Major)
Fresh baseline 31,256/4 (fixed); one load-sensitive test; no per-release status artifact; RCH fleet drift.
`proof_category: infrastructure`

### G60 — README demo assets — PARTIAL (Minor)
Embeds removed; no recording exists.
`proof_category: process`

### G61 — Documentation drift sweep — WORKING (closeable)
`proof_category: process`

### G62 — Upstream WezTerm backport cadence stalled — REGRESSED (Major)
`proof_category: process`

### G63 — Bead graph as a truthful ready queue — PARTIAL (Major)
P0 434 → 25; 71 stale claims released; vertical-slice rule and weekly audit not yet in force.
`proof_category: coordination`

### G64 — Orphaned `mcp_helpers.rs` — WORKING (closeable once the guard lane is green)
`proof_category: process`

### G65 — `main.rs` monolith (135,372 lines) — PARTIAL (Major maintainability)
`proof_category: substrate`

### G66 — CLI/GUI/mux version-skew contract — WORKING in code (dev) (folded into G51)
`proof_category: 4 (conformance-artifact)`

### G67 — Rule-pack currency has no cadence — REGRESSED (Major; upgraded from UNPROVEN)
The flagship `codex.usage.reached` regex missed the real binary's casing; fixtures never contained the real string.
`proof_category: 4 (conformance-artifact)`

### G68 — Tantivy secondary index / hybrid ranking wiring — UNPROVEN (Minor)
`proof_category: 4 (conformance-artifact)`

### G69 — Surfaces this pass could not verify — UNPROVEN (Major, sweep)
Incident-bundle live collectors, notification backends, connector certification pipeline, IPC auth, `ft mission run` dispatch, backup/restore e2e, session dump/recover, `ft get-text` tail semantics (returned blank lines for a pane with output in §11).
`proof_category: 4 (conformance-artifact)`

### G70 — Formal-method packaging — Minor
`proof_category: 6 (formal-method) + 12 (mechanized-proof)`

### G71 — Mux listener aborted on every client connect (HEAD, GUI and headless) — FIXED in tree, UNPROVEN (Critical)
**Reality:** `LocalListener::run` spawned the per-connection future with `spawn_local` from the listener thread; async_task's thread check panicked on the main thread and again in drop → SIGABRT on first connect. Fixed by `admit_connection` → `handoff_to_main_thread_local` (647d87fd6) with a regression test that aborted under the old code. No remote proof yet; no released GUI carries the fix.
`proof_category: 4 (conformance-artifact)`

### G72 — Direct mux client poisons itself on a benign broadcast during the handshake — FIXED in tree, UNPROVEN (Major)
**Reality:** a pane spawning during registration makes the mux broadcast `TabResized` (serial 0); the phase gate treated it as an inbound authority violation. Fixed (0f00b7cf5) with a scripted-server test.
`proof_category: 4 (conformance-artifact) + 2 (property-test)`

### G73 — Flagship rule regex vs real Codex output — FIXED in tree (Major) → folds into G67
`(?i)` regex + test on the real string (e903495ee).

### G74 — Writer drops a segment under intra-process contention; sequence cursor never re-converges — HALF FIXED (P1)
**Reality:** deferred `BEGIN` + read-then-write → immediate SQLITE_BUSY bypassing busy_timeout; group commits now `BEGIN IMMEDIATE` (7a30c6560, tests updated, unproven live). The capture-side `bounded_segment.seq` counter is never realigned after a drop, so the discontinuity warning fires on every later segment. Snapshot-engine startup also logs `error_class="database"` warnings that share the same root.
`proof_category: 2 (property-test) + 4 (conformance-artifact)`

### G75 — `frankenterm-mux-server` has no logger and silently ignores `--config-file` — Minor (operability)
**Reality:** the binary never installs a logger; config load errors vanish; it always binds `RUNTIME_DIR/sock`. Discovered while trying to isolate the smoke to a temp socket.
`proof_category: process + 4`

### G76 — `ft send` default paste mode does not execute in a shell — Minor (contract clarity)
**Reality:** bracketed paste is right for agent TUIs and wrong for shells; nothing documents the difference; the smoke needed `--no-paste`.
`proof_category: process`

### G77 — Snapshot engine startup contends with the capture loop — Minor (folds into G74)

### G78 — Four more tracked orphan source files (`cx_stub.rs`, `search/model2vec_embedder.rs`, `storage/handle/mod.rs`, `test_subprocess_deadlock.rs`) — Minor (authorization)
Baselined in `KNOWN_ORPHANS`; deletion needs owner authorization (ft-xxfwy.31).
`proof_category: process`

---

## 4. Closure plan (Phase 2 deep pass, revised in place 2026-09-02)

Ordered by vision impact. Every section has the same shape so it can be carried verbatim into its beads: **Status now** (what is on `main` and what is proven), **Target state** (the observable a stranger can check), **Design** (exact modules, data shapes, error codes, operator surfaces), **Work breakdown** (granular, with done/open markers), **Tests and evidence** (unit, property, e2e with structured logging and a retained artifact path), **Acceptance** (positive observable, planted negative, no-claim line), **Dependencies**, **Risks and degradation**, **Effort**, **Beads**.

Conventions used throughout:
- "proof" = a retained remote RCH lane log or a signed attestation slot; "dev signal" = a local run on the maintainer's Mac. Dev signals never close a bead.
- Every e2e script writes a JSON receipt (`schema`, `generated_at`, `host`, `cli_version`, `mux_version`, `commit`, `status`, `steps[]`) to a path named in its section, logs every step with timestamps to a sibling `.log`, and exits non-zero on any failed step. Skipped steps carry a `reason`; a receipt with any skipped step is never `status: pass`.
- Artifacts that make a README claim true live under `docs/attestations/proofs/` and are referenced from the attestation bundle slot named in the section.

### G50 — Signed, installable, activated release

**Status now.** `scripts/release/verify-release.sh <tag>` exists and exits 1 on v0.15.1 (unsigned). `install.sh` has `--activate <64hex> --idle-host-confirmed` and `activate_process_family_generation()` (refuses on a live mux, refuses without the confirmation flag, promotes the candidate selector otherwise); `dsr signing sign` works with the maintainer's minisign key. No signed release exists; no activation e2e exists.

**Target state.** On a clean Apple-Silicon Mac and a clean Ubuntu host, `curl -fsSL <install-url> | bash` ends with `ft --version`, `ft doctor --json` (`ok: true`), and `frankenterm-mux-server --version` on PATH, from a generation whose every artifact has a `.minisig` verified against `release/minisign.pub`, and `install-receipt.json` shows `activation: current`. On a host with a live mux the installer stops at `pending` and prints one exact command (the `--activate … --idle-host-confirmed` line) plus the reason.

**Design.**
1. Signing lives in the DSR release step, not in a script that a human might forget: `~/.config/dsr/repos.yaml` `frankenterm` entry gets a post-build hook `scripts/release/sign-artifacts.sh <dist-dir>` that runs `minisign -S -s <key> -m <asset>` for every artifact, writes `<asset>.minisig`, and rewrites `frankenterm-v<version>-manifest.json` with `signed: true`, `signature_file`, `signature_sha256`. The hook fails closed if the key is absent (`FT_RELEASE_SIGNING_KEY` or `~/.config/dsr/secrets/minisign.key`).
2. `scripts/release/verify-release.sh` becomes a DSR `required_checks` post-publish step: downloads the manifest and every asset+signature with the standard curl user agent, verifies each with `minisign -V -p release/minisign.pub`, checks the manifest's `signed` flags and SHA-256s, and exits 1 on any miss. Output is a JSON receipt at `docs/attestations/proofs/release-verify-<version>.json`.
3. The installer's verification path is unchanged; the new contract is that `--no-verify` prints a red banner and writes `verification: skipped` into `install-receipt.json` so a downstream `ft doctor` row ("install verification") can show it.
4. Activation: keep the two tiers. Idle host (no mux socket lease held): installer runs `activate_process_family_generation` automatically when `--activate-if-idle` is passed or when the receipt's `authority` is `initial` (first install), then re-runs the installed `ft doctor` and fails the install if activation is still pending. Live host: stays pending; receipt `next_action` carries the exact command. The live-host lease handoff remains `7xqz4.12`'s scope and is referenced, not duplicated.
5. README Quick Install gains a two-line "what you will see" block (receipt path, activation state) and drops the pending path from the happy-path narrative.

**Work breakdown.**
- [done] `verify-release.sh` (release verification, exits 1 on unsigned).
- [done] `install.sh --activate <gen> --idle-host-confirmed`, refusal paths, summary next_action.
- [open] `scripts/release/sign-artifacts.sh` + DSR hook wiring (`repos.yaml`), fail-closed on missing key.
- [open] `verify-release.sh` receipt output + DSR post-publish check.
- [open] `--activate-if-idle` + first-install auto-activation + post-activation `ft doctor` gate inside `install.sh`.
- [open] `install-receipt.json` fields: `verification`, `activation`, `next_action`, `generation`, `selector_path`.
- [open] README Quick Install rewrite (after .2 ships).
- [open] Operator step: `dsr release` v0.15.2 with signing on; never touch historical unsigned assets.

**Tests and evidence.**
- Unit (bash, `tests/installer/`): manifest rewriting; `signed:false` rejection; `--no-verify` banner+receipt field; idle/live activation branches with a fake lease file; failpoint rollback (`FT_INSTALL_FAILPOINT=after-selector-swap` must leave the previous `current` intact).
- E2E `tests/e2e/test_install_signed_clean_host.sh`: temp `$HOME`, offline tarball + `.minisig` + `minisign.pub`, runs the real `install.sh`, asserts PATH binaries, `ft doctor --json .ok == true`, receipt `activation: current`; planted negative: a tampered byte in the tarball must fail with the verification error and leave no `current` selector. Receipt: `docs/attestations/proofs/install-e2e-<version>-<os>.json`.
- E2E live-host variant: start a `frankenterm-mux-server` first; assert `pending` + `next_action` and that the printed command activates after the server stops.

**Acceptance.** Positive: both receipts `status: pass` for v0.15.2 on macOS arm64 and Ubuntu x86_64, bound to the release SHA. Planted negative: tampered tarball → exit 1, receipt `status: fail`, reason `signature_mismatch`. No-claim: this proves the documented path, not the live-host guardian handoff.

**Dependencies.** Signing (.1) → release (.2); activation code (.3) → e2e (.4); .30 waits on .2/.3. **Risks.** Key custody (operator-only; never in the repo); DSR hook ordering (sign before checksum manifest is finalized); `minisign` absence on the target host (installer already errors clearly). **Effort.** .1 S, .2 M (operator), .3 M, .4 M. **Beads.** ft-xxfwy.1, .2, .3, .4 (+ .30 gate).

### G51 / G66 — `ft` finds and talks to the running FrankenTerm.app; version skew is a contract

**Status now.** On `main`: `config::gui_socket` (shared naming: `GUI_SOCKET_PREFIX`, `DEFAULT_WINDOW_CLASS`, `published_gui_sock_path`, `resolve_published_gui_sock`, `list_gui_socket_entries`), `wezterm::discover_mux_socket_ranked` returning `DiscoveredMuxSocket { path, source: MuxSocketSource }` ordered ExplicitConfig → Environment → GuiPublished → GuiInstance → ConfigUnixDomain → DefaultUnixDomain, dangling symlinks and non-accepting sockets rejected; `build_unified_client` dials the discovered path; `ft doctor` row `mux socket … (source: …)`; `WeztermError::VersionSkew { local_codec, local_min, remote_codec, remote_min, remote_version }` → `robot.mux_version_skew` (FT-1025) with the pairing recommendation. Proven: client discovery 10, core `wezterm::tests` 197, config `gui_socket` 4 (hz2); lane 8 on the final tree 1000/1 (the one failure is unrelated, G59). Dev signal: doctor shows `source: gui_published` against the 0.13.0 app and reports skew; the headless same-generation smoke passes (G71/G72 fixes required).

**Target state.** With FrankenTerm.app running and no config, `ft robot state` lists its panes within 2 s on a shipped pair; with a skewed pair every robot envelope carries `robot.mux_version_skew` naming both generations and the install command; `ft doctor` shows which socket was chosen and why.

**Design (remaining).**
1. Attach e2e must run against a **shipped** app: build the app from the same tag as the CLI (`frankenterm-gui` release lane on the Mac, per `native-dev-build-recipe`), install it as a second bundle id (`com.dicklesworthstone.frankenterm.rc`) so the daily-driver 0.13.0 app is untouched, launch it headless (`open -a … --args --skip-config`), then run the smoke against `default-<rc bundle id>` via `GuiPublished`.
2. Differential test: pane list via the GUI socket vs via `frankenterm-mux-server` from the same build must match on `(pane_id, tab_id, window_id, domain, cwd)`.
3. Skew contract test: a scripted server answering `GetCodecVersion` with `codec_vers = CODEC_VERSION + 3, min_supported = CODEC_VERSION + 1` must produce `robot.mux_version_skew` with the exact remote/local numbers and a `recommendation` string containing the install command; the installer prints the same command when it detects a skewed installed app (`install.sh` reads `Contents/Info.plist` `CFBundleShortVersionString` and compares codec generation via `ft version --json .codec_version`).

**Work breakdown.**
- [done] discovery, doctor row, typed skew, robot code, docs.
- [open] `ft version --json` exposes `codec_version` and `codec_min_supported` (if not already; verify) so the installer can compare.
- [open] installer skew warning + pairing command (G66 installer half).
- [open] scripted-server skew test (unit) in `vendored::mux_client::tests`.
- [open] `tests/e2e/test_ft_attaches_to_running_gui.sh` (native macOS, second bundle id) writing `docs/attestations/proofs/gui-attach-<version>.json`.
- [open] differential pane-list test in the same script.

**Tests and evidence.** As above; the e2e receipt binds `cli_version`, `app_version`, `codec_version`, `bundle_path`, and the socket `source`. Logging: every `ft` call in the script runs with `-v` and stderr captured to the `.log`.

**Acceptance.** Positive: receipt `status: pass` with a non-empty pane list and `source: gui_published`. Planted negative: run the same script against the 0.13.0 app and assert `robot.mux_version_skew` with `remote_codec < local_min`. No-claim: does not prove agent detection (G52) or performance.

**Dependencies.** G71 fix released in the app generation under test. **Risks.** Building the GUI on the Mac takes ~1 h; the second bundle id needs its own runtime dir class (already supported by `DEFAULT_WINDOW_CLASS` parameterization). **Effort.** M. **Beads.** ft-xxfwy.5 (closeable on lane 8), .6, .7, .8.

### G52 — Live-agent proof of the loop (the product's existence proof)

**Status now.** `scripts/smoke/headless-mux-observe.sh` passes on a dev build: one `zsh -f` pane, title forced to `codex`, the real Codex usage-limit line echoed, `codex.usage.reached` detected with `reset_time`. No agents, no `--auto-handle`, no signed artifact, and the receipt is the script's stdout, not a JSON file yet.

**Target state.** Three retained, redacted, signed proof artifacts (`docs/attestations/proofs/live-loop-tier{1,2,3}.json`): tier 1 = three real agent panes (Claude Code, Codex, Gemini) inside a shipped FrankenTerm.app or headless mux from the same generation, a real usage-limit or compaction event per agent, `ft watch --auto-handle` reacting through the policy gate, the audit row and the workflow run visible via `ft robot events` / `ft audit`; tier 2 = 20 panes with FTS search and one policy denial; tier 3 = 50 panes with fleet-pressure tier transitions asserted from `ft robot fleet` output, not logged.

**Design.**
1. `scripts/live-loop-proof.sh <tier>` (bash + jq) stages panes with `ft robot profile create/apply` (profile create landed 5945f1127), waits for each agent banner rule (`*.banner`) to fire, injects the trigger (for Codex: run the agent to its limit is not reproducible on demand — use the agent's own `--help`/banner for presence and a **replayed fixture** for the limit line with `adapter: live-pane-fixture-injection` clearly recorded; a true limit event, when one happens naturally, upgrades the artifact to `adapter: live`), asserts the event, the workflow run, the audit row, and writes the receipt. The receipt has `assertions[]` each with `kind: authoritative|informational`; the script fails if any authoritative assertion is not `pass`, and `ft-d0ez0.5`'s unconditional assertions become authoritative ones here.
2. Recorder: run with the recorder enabled so the session can be replayed (`ft record export`), and store the recording hash in the receipt.
3. Attestation: the bundle gains slots `live_loop_tier1..3` whose evidence is the receipt path + SHA; `scripts/attestation-build.sh` validates presence and `status`.
4. Redaction: receipts pass through `ft redact` before commit; the script refuses to write a receipt containing an API key pattern.

**Work breakdown.**
- [done] headless smoke (one pane, no agents).
- [open] JSON receipt + `.log` for the smoke (reuse as the tier-0 fixture).
- [open] `scripts/live-loop-proof.sh` tier 1 with profile staging, banner wait, fixture injection, `--auto-handle`, assertions.
- [open] tier 2/3 extensions (pane counts, FTS search assertion, policy denial assertion, fleet tier assertion via `ft robot fleet --json`).
- [open] attestation slots + builder validation.
- [open] retire `ft-d0ez0.1-.3` semantics: comment on `ft-d0ez0` linking the new receipts; close `.5` only when tier 3 is authoritative.

**Tests and evidence.** The scripts are the tests; each run's receipt and log are retained under `docs/attestations/proofs/` and referenced from the bundle. Unit coverage for the receipt schema (`serde` round-trip in `frankenterm-core::attestation`).

**Acceptance.** Positive: three receipts `status: pass`, every assertion authoritative, `adapter` recorded. Planted negative: run tier 1 with `--no-auto-handle` and assert the receipt fails on the "workflow ran" assertion. No-claim: fixture-injected limit lines prove the pipeline, not the agent's real output cadence (that is G67).

**Dependencies.** G51 attach e2e (.8) and G55 (.12) for sends to pass the policy gate. **Risks.** Agents' TUIs consume pasted input differently (use `--no-paste` only for shells); rate limits are not reproducible on demand (fixture injection, disclosed). **Effort.** XL (tier 1 L, tiers 2/3 L). **Beads.** ft-xxfwy.9, .10.

### G53 — Dogfooding as a standing gate

**Status now.** `scripts/dogfood-status.sh` prints per-db capture age, event and workflow counts, and a `stale|fresh` verdict (stale today). No watcher runs continuously.

**Target state.** A launchd agent keeps `ft watch` running on the maintainer's workspaces; the verdict is `fresh` (< 24 h) whenever a release is cut, and the attestation bundle's `dogfood_status` slot carries the receipt.

**Design.** `scripts/dogfood/ft-watch.launchd.plist` template (label `com.dicklesworthstone.frankenterm.watch`, `KeepAlive`, `WorkingDirectory` = workspace, `StandardOutPath` under `~/Library/Logs/frankenterm/`); `ft doctor` row `dogfood` reading the same computation as the script (shared Rust function `dogfood::status(db_paths)` so script and doctor cannot disagree); `scripts/release-gates.sh` gains `gate dogfood-status` (advisory until the first signed release, then blocking).

**Work breakdown.** [done] status script; [open] shared Rust status function + doctor row; [open] launchd template + `scripts/dogfood/install.sh`; [open] attestation slot; [open] release gate (advisory→blocking switch recorded in the bundle).

**Tests and evidence.** Unit test for the status computation with a fixture db (fresh/stale/no-db); the release receipt.

**Acceptance.** Positive: `dogfood-status.sh --json .verdict == "fresh"` on release day, receipt in bundle. Planted negative: a db whose newest capture is 25 h old → `stale`. No-claim: freshness proves the loop ran, not that detections were correct.

**Dependencies.** G51 (the watcher must attach to the daily app). **Effort.** S. **Beads.** ft-xxfwy.11.

### G54 — First real signed attestation bundle; DSR runs the gates

**Status now.** 25 GitHub workflow files retired (owner-authorized); `scripts/release-gates.sh` runs 28 static gates (`--list`, `--only`, `--cargo` for the cargo-class gates), all green after re-blessing three baselines; the attestation gate builds the dev bundle then verifies it (a disclosed weakening: hash drift passes by construction; structure/signature/retraction checks remain). DSR `required_checks` run only `cargo check/clippy/test/fmt`.

**Target state.** `docs/attestations/0.15.2.json` exists, is minisign-signed, verifies offline with `scripts/attestation-verify.sh --strict-deferred` and `ft attestation verify`, and DSR refuses to publish when `scripts/release-gates.sh` or the verifier fails.

**Design.**
1. `repos.yaml` `required_checks` += `scripts/release-gates.sh --cargo` (runs on the RCH worker like the existing checks; static gates run locally in seconds) and `scripts/attestation-verify.sh --strict-deferred docs/attestations/<version>.json`.
2. `scripts/attestation-build.sh --release <version>` fills the slots from retained receipts (G50 install e2e, G51 attach, G52 tiers, G53 dogfood, G58 headline claims, G59 suite status, G70 formal lanes); every slot is `pass`, `deferred_to_bead: <id>` with a reason, or `fail`; `--strict-deferred` fails on any deferred slot unless the bead is open and P0/P1.
3. Signing of the bundle reuses the release key (`minisign -S`), signature stored next to the bundle; README tour cites `docs/attestations/<latest>.json` (already fixed to `<version>`).
4. The attestation gate's build-then-verify is replaced by verify-only once a signed release bundle exists; the dev bundle stays gitignored.

**Work breakdown.** [done] workflow retirement, gates script, README path; [open] DSR wiring of both checks; [open] slot filling from receipts + strict-deferred semantics; [open] bundle signing; [open] first signed bundle at v0.15.2; [open] switch the attestation gate to verify-only and record the switch.

**Tests and evidence.** `tests/attestation/` fixtures already cover the verifier; add a fixture bundle with one deferred slot to prove `--strict-deferred` fails; the DSR run log for v0.15.2 retained under `docs/attestations/proofs/dsr-quality-0.15.2.log`.

**Acceptance.** Positive: `ft attestation verify docs/attestations/0.15.2.json` → ok; `dsr release` log shows both checks. Planted negative: a bundle with a `deferred` slot whose bead is closed must fail strict verification. No-claim: a verified bundle proves the listed evidence exists and is signed, not that the evidence is sufficient (that is what each slot's receipt claims).

**Dependencies.** G50 signing (.1); G59 suite status (.17). **Effort.** M. **Beads.** ft-xxfwy.15, .16 (DSR wiring remainder).

### G55 — Prompt-active evidence everywhere; the tour is honest

**Status now.** Five CLI tx sites call `resolve_tx_contract_capabilities` so PromptActive preconditions can pass when OSC-133 evidence exists; the robot/MCP paths already pre-resolve (ft-g2ari). **Correction 2026-09-02:** the shell-integration installer already exists as `ft setup shell` (bash/zsh/fish, `--dry-run`, `--apply`, `--remove`, `--rc-path`, idempotent managed block); the 2026-09-01 plan wrongly listed it as missing. What is actually missing: the README never mentions `ft setup shell`, `policy.prompt_unknown` carries no hint pointing at it, and there is no per-pane evidence row in doctor.

**Target state.** A pane with shell integration sends `ok: true`; a pane without it gets `policy.prompt_unknown` with `hint: "run ft setup shell-integration"`; `ft doctor --json` lists per-pane `prompt_evidence: osc133|none|stale` with age; the README tour shows the RequireApproval path for the first send unless integration is present.

**Design.** `ft setup shell-integration [--shell zsh|bash|fish] [--print]` installs `~/.config/frankenterm/shell-integration.<shell>` (bundled OSC-133 scripts already used by the GUI) and appends a guarded source line to the rc file (idempotent, marker comment, `--print` for manual use); `capabilities.prompt_active` gains `evidence_age_ms`; doctor row per observed pane; envelope hint string constant in `robot_types.rs`.

**Work breakdown.** [done] CLI capability resolution; [open] `ft setup shell-integration`; [open] evidence age + doctor rows; [open] hint on `policy.prompt_unknown`; [open] README tour rewrite; [open] proptest over capability resolution (`resolve_pane_capabilities` with arbitrary segment/OSC histories → never panics, monotone evidence age); [open] two-pane e2e (integration vs none) writing `docs/attestations/proofs/prompt-evidence-e2e.json`; [open] `ft tx run` precondition e2e on a real pane.

**Acceptance.** Positive: e2e receipt with both outcomes. Planted negative: stale evidence (older than the configured max) → `prompt_unknown`. No-claim: does not prove agents' TUIs emit OSC-133 (they don't; agents rely on the pane's shell wrapper).

**Dependencies.** none for the code; G51 for the e2e host. **Effort.** M. **Beads.** ft-xxfwy.12, .13.

### G56 — Kill-switch tiers proven and visible

**Status now.** Fix f8c674376 committed in June; doctor rows now say `[process-local: fresh engine for this doctor run; the watcher's live policy state is not persisted]`; tests queued on remote lanes.

**Target state.** Tier × action-kind conformance matrix as `docs/attestations/proofs/killswitch-tier-enforcement.json`; the watcher persists its kill-switch state (`policy_state` table row per workspace with tier, reason, set_at, actor) and `ft doctor` / `ft robot policy state` read the persisted tier; SoftStop/HardStop fail closed for workflow/connector/file/exec.

**Finding (2026-09-02).** `PolicyEngine::trip_kill_switch` has no production caller: only tests trip it. The robot/MCP `kill_switch` arguments are `plan::MissionKillSwitchLevel` (mission-scoped), not the policy engine's quarantine kill switch. The tier enforcement fixed by `ft-l59nq` therefore guards a switch that no CLI, robot, MCP, or IPC surface can flip, and the doctor's "kill switch disarmed" row is true by construction.

**Design.** (1) Surface first: `ft robot policy kill-switch trip --level soft|hard|emergency --reason <text>` and `… reset`, delivered to the running watcher over IPC (the CLI never trips a process-local engine), with the audit-chain entry the engine already writes. (2) Persistence through the generic config-KV surface (key `policy.kill_switch`, JSON `{level, reason, by, set_at_ms}`), written by the watcher on every trip/reset and read at engine construction so a restarted watcher comes up in the same tier (fail closed on a corrupt value: HardStop + log). (3) Doctor and `ft robot policy state` read the persisted row and show level, reason, actor, age; the process-local caveat is dropped only when the row exists. (4) Only then the tier × action-kind conformance matrix, run against a switch tripped through the surface.

**Work breakdown.** [open] green lane for the `ft-l59nq` tests; [open] conformance matrix test generating the JSON artifact; [open] persistence via config-KV + read-at-construction; [open] doctor/robot rows.

**Acceptance.** Positive: matrix artifact all `deny` for the guarded kinds under SoftStop/HardStop; doctor shows the watcher-set tier from a separate process. Planted negative: corrupt the persisted value → engine falls back to HardStop (fail closed) and logs. No-claim: persistence does not make the watcher restart-safe for in-flight actions.

**Effort.** S+M. **Beads.** ft-xxfwy.14.

### G57 — `/stream/events` carries live events

**Status now.** `StorageEventTail` polls `events` by id cursor (250 ms, batch 256) and publishes to the `EventBus` in standalone `ft web` (default on, `with_storage_event_tail(false)` to disable); stopped on shutdown. Lane pending; no e2e.

**Target state.** A detection appears on `GET /stream/events` within 1 s in both modes (standalone `ft web`; `ft watch --web` in-process), with lag frames when a client falls behind, filters, and redaction.

**Design.** In-process mode: when `ft watch --web` runs, the runtime's bus is shared (no tail); standalone: tail. SSE frames `event: detection|lag|heartbeat`, `id:` = event id for `Last-Event-ID` resume; filters `?pane_id=&rule_id=&since_id=`; redaction via the same `Redactor` the CLI uses.

**Work breakdown.** [done] tail + config + shutdown; [open] `ft watch --web` sharing the runtime bus; [open] `Last-Event-ID` resume + lag frames; [open] filters; [open] e2e `tests/e2e/test_web_sse_live_events.sh` (starts `ft web` on a temp db, inserts an event via `ft event ingest`, curls the stream with a 3 s timeout, asserts the frame) writing `docs/attestations/proofs/web-sse-e2e.json`.

**Acceptance.** Positive: frame within 1 s in both modes. Planted negative: a client that reads nothing for 5 s receives a `lag` frame and no dropped-silently frames. No-claim: not a load test.

**Effort.** M. **Beads.** ft-xxfwy.18, .19.

### G58 — Measured headline claims per release

**Status now.** `docs/perf/headline-claims.json` defines `capture_latency_p99`, `concurrent_panes_capacity`, `memory_per_pane_budget`, `zstd_compression_ratio`, `bloom_prefilter_speedup` with a `publishing` block; `docs/perf/releases/` has only a README; the load rig is synthetic.

**Target state.** `docs/perf/releases/<version>.headline-claims.json` produced by `scripts/perf/headline-run.sh` on the declared local M-series baseline (quiet host; cv ≤ 5 % gate) with distributions, the `ebci-upper-bound` gate applied, and the attestation slot pointing at it.

**Design.** One bench per claim (existing criterion benches where they exist: `capture_latency`, `bloom_prefilter`; new: a real-pipeline 200-pane rig using `frankenterm-mux-server` + `ft watch` with synthetic *output* but real capture/persist/detect code paths, memory measured via `ps`/`task_info`); the script refuses to run unless the host is quiet (load average and no other cargo processes) and records `host_fingerprint`.

**Work breakdown.** [open] `headline-run.sh` + per-claim runners; [open] real-pipeline rig (retire `ft-7h5da.10.3.2` synthetic metrics); [open] cockpit wiring (`ft-7h5da.10.4.3`); [open] attestation slot + README numbers stamped from the artifact by `stamp-readme-counts.sh`.

**Acceptance.** Positive: artifact with five claims, each `pass|fail|deferred` with numbers and cv. Planted negative: run on a busy host → script exits 2 with `host_not_quiet`. No-claim: local M-series numbers, not a fleet SLO.

**Dependencies.** G52 rig staging. **Effort.** L. **Beads.** ft-xxfwy.20.

### G59 — Known test-suite state per release; a fleet that can prove

**Status now.** Fresh core-lib baseline 31,256/4 (all four fixed); lane 8 on the final tree 1000/1 where the failure is `retry::tests::retry_runs_deterministically_under_labruntime_with_cx` measuring 5.4 s wall time on a loaded worker (passes in 0.19 s locally); `TMPDIR=/tmp` is required on RCH; hz2 vanished from the fleet mid-session.

**Target state.** `docs/attestations/doctrine/test-suite-status.json` per release (pass/fail/ignored counts, worker, SHA, duration) produced by the DSR quality run; zero known-red tests or each `#[ignore]` links a bead; load-sensitive wall-clock assertions replaced by virtual-time assertions.

**Design.** `scripts/test-suite-status.sh` wraps the remote lane and writes the artifact from the `test result:` line plus `--format json` when available; the retry test asserts on the lab runtime's virtual clock (`report.virtual_elapsed`) instead of `Instant`; fix `ft-ziprn` at the source (the test helpers pick `TMPDIR` under the workspace `.rch-tmp` — make the helper fall back to `/tmp` when the workspace path is not writable/mounted).

**Work breakdown.** [done] baseline + four fixes; [open] retry test virtual-time assertion; [open] `ft-ziprn` source fix; [open] status script + DSR wiring + attestation slot; [open] fleet note in AGENTS.md (no worker pins; `TMPDIR=/tmp`).

**Acceptance.** Positive: artifact for v0.15.2 with `failed: 0`. Planted negative: inject a failing test on a branch → artifact `failed: 1` and DSR refuses. No-claim: core-lib only until the workspace suite is green.

**Effort.** M. **Beads.** ft-xxfwy.17.

### G60 — Demo recordings

**Status now.** README embeds removed; `scripts/demo.tape` and `demo-full.tape` exist.

**Target state.** `assets/demo.gif` rendered from `demo.tape` with vhs against a dev build on the headless mux (the smoke recipe makes this reproducible); `demo-full` stays out until G52 tier 1 exists; `scripts/check-readme-assets.sh` fails when README references a missing asset.

**Work breakdown.** [open] render + commit `demo.gif` (size-capped); [open] asset check in `release-gates.sh`; [open] re-add the embed.

**Acceptance.** Positive: gate passes with the embed present. Planted negative: reference a missing asset → gate fails. **Effort.** S. **Beads.** ft-xxfwy.21.

### G61 — Documentation truth sweep — closeable

All enumerated corrections landed (schema v45, `<version>.json`, DSR wording, detector, `metrics` row, Stateright wording, FAQ path, demo embeds, `--activate` row, web tail note, `robot.mux_version_skew`, `profile create`, doctor process-local clause). Remaining: `stamp-readme-counts.sh` stamps `SCHEMA_VERSION` (open, S). **Acceptance.** `stamp-readme-counts.sh --check` green (it is). **Beads.** ft-xxfwy.22 (close after the stamp extension or now with the extension moved to .22's comment).

### G62 — Upstream WezTerm backport cadence

**Status now.** Last `Upstream-WezTerm:` commit 2026-05-13; `PROVENANCE.json` has `divergence_point`, `fork_side_commit_total`, `vendored_crate_count` but no batch record.

**Target state.** Backports resume weekly per AGENTS.md steps 1–10; `PROVENANCE.json.backport_batches[]` records `{upstream_range, applied_shas[], skipped[], date}`; `scripts/check-upstream-backport-due.sh` (advisory, 14 days) is part of `reality-check-status.sh`.

**Design.** First batch: enumerate upstream commits 2026-05-13 → now for the vendored crates, classify (apply / skip-with-reason / conflicts), apply in per-crate commits with `Upstream-WezTerm:` trailers, run the crate lanes; record the batch. Later batches are S each.

**Acceptance.** Positive: a batch record and ≥ 1 applied commit per week. Planted negative: the due-check reports overdue when the newest batch is > 14 days old. **Effort.** first batch L, then S. **Beads.** ft-xxfwy.23.

### G63 — Truthful ready queue

**Status now.** 606 P0/P1 children demoted to P2 (open P0 = 25), 71 stale claims released, 26 broadcast. Not done: vertical-slice rule, weekly silent-close audit, create/close trend.

**Target state.** Open P0 ≤ 25 and every P0 names the release it gates; no in_progress bead idle > 14 days; every new epic declares a first vertical slice; weekly `scripts/ft-reality-check.sh silent-close-audit` output attached to the drumbeat report.

**Work breakdown.** [done] demotion + stale release; [open] `check-reality-check-bead-structure.sh` extension: epic must have a `first_vertical_slice:` line; [open] weekly audit in `reality-check-status.sh`; [open] drumbeat report line for create/close per week.

**Acceptance.** Positive: gate output shows the three invariants. Planted negative: an epic without the slice line fails the gate. **Effort.** S. **Beads.** ft-xxfwy.24.

### G64 — Orphan removed with authorization — closeable on a green guard lane

Done: `mcp_helpers.rs` removed (authorized), `tests/no_orphan_source_files.rs` guard, census re-blessed. Remaining: the guard lane (lanes-11) green. **Beads.** ft-xxfwy.25.

### G65 — `main.rs` split isomorphically

**Status now.** 135,372 lines (grew 5.6 k since 2026-09-01 through sibling work).

**Target state.** `crates/frankenterm/src/commands/<family>.rs` per command family (robot, watch, mission, tx, session, attestation, setup, doctor…), each file < 15 k lines, with the golden robot envelope matrix (`tests/robot_api_contracts.rs` + the `docs/robot-contracts/` fixtures) proving zero behavior drift.

**Design.** Measure seams with `rg '^    [A-Z][A-Za-z]+ \{' main.rs` grouped by enum; move one family per commit with `pub(crate)` visibility and no signature changes; the golden matrix runs before and after each move on the same worker; sequence robot → watch → mission/tx → session → attestation → the rest; forbid new functions in `main.rs` via a ratchet (`scripts/check-main-rs-size.sh` with a baseline that can only shrink).

**Acceptance.** Positive: each move's lane green + matrix identical. Planted negative: a deliberate envelope change in a family module → matrix fails. No-claim: not a refactor of behavior. **Effort.** XL (staged, 8–10 commits). **Beads.** ft-xxfwy.26.

### G67 / G73 — Rule-pack currency with real-binary provenance

**Status now.** `codex.usage.reached` regex is now `(?i)` with a test on the real string; the corpus still has no fixture sourced from the shipped binaries.

**Target state.** Every rule in the 12 highest-value set has at least one fixture whose provenance line names the agent binary version and how it was captured (`strings <binary>` or a redacted live capture); `scripts/check-rule-corpus-age.sh` warns when the newest provenance is > 30 days; monthly refresh bead template.

**Design.** Fixture header `# provenance: codex 0.133.0 strings 2026-09-02` parsed by `ft robot rules lint --fixtures --strict`; a `scripts/rules/harvest-binary-strings.sh <agent>` helper that greps the installed binaries for each rule's anchors and writes candidate fixtures for human review.

**Acceptance.** Positive: lint shows provenance for the 12 rules; corpus-age check green. Planted negative: a fixture whose regex no longer matches the harvested string fails lint. **Effort.** S recurring (first pass M). **Beads.** ft-xxfwy.27.

### G68 / G69 — Verification sweep of unverified surfaces

**Status now.** One finding acted on (profile create); the rest unverified. New item from §11: `ft get-text <pane>` returned six blank lines for a pane that had output (tail semantics or off-by-screen bug — unverified).

**Target state.** `docs/attestations/proofs/surface-verification.json` with one entry per surface (`incident_bundle_collectors`, `notification_backends`, `connector_certification`, `ipc_auth`, `mission_run_dispatch`, `backup_restore_e2e`, `session_dump_recover`, `tantivy_hybrid`, `get_text_tail`) each `pass|blocked|skipped` with a named check command and artifact.

**Design.** `scripts/verify-surfaces.sh` runs each check against the headless mux fixture where a mux is needed; every `skipped` carries a reason and a bead; README "Supported" rows link the entry.

**Acceptance.** Positive: no README "Supported" surface without an entry. Planted negative: a check whose command exits non-zero is recorded `fail`, never `skipped`. **Effort.** M. **Beads.** ft-xxfwy.28.

### G70 — Formal-method packaging

**Target state.** `scripts/release-gates.sh --cargo` includes the Loom lane and the Lean soundness check with retained logs; `docs/attestations/proofs/formal-lanes.json` lists spec → check → result → log. **Effort.** S. **Beads.** ft-xxfwy.29.

### G71 — Mux listener abort on connect (fixed; needs proof and a released generation)

**Status now.** Fix 647d87fd6 (`admit_connection` via `handoff_to_main_thread_local`), regression test `local::tests::accepted_connection_is_admitted_on_the_main_thread_from_the_listener_thread`; headless smoke passes; lanes-11 pending.

**Target state.** Remote lane green for `frankenterm-mux-server-impl local::tests`; the headless smoke is a DSR quality step on the Mac lane (`scripts/smoke/headless-mux-observe.sh target/release`) so a listener regression can never ship; the next app release (v0.15.2) carries the fix and the GUI attach e2e (G51) proves it.

**Work breakdown.** [open] lane green; [open] smoke as a DSR check with a JSON receipt; [open] a `frankenterm-gui` unit test that the GUI's `spawn_mux_server` path also reaches `admit_connection` (or shares `LocalListener::run` — it does; assert by test that no other `spawn_local` call exists in listener threads: a source-scan test in `mux-server-impl` forbidding `reservation.spawn_local(` outside `admit_connection`).

**Acceptance.** Positive: lane + smoke receipt green at v0.15.2. Planted negative: reintroduce the direct `spawn_local` on a branch → the regression test aborts. **Effort.** S. **Beads.** ft-xxfwy.33.

### G72 — Handshake tolerates unilateral notifications (fixed; needs proof)

**Status now.** Fix 0f00b7cf5; test `vendored::mux_client::tests::unilateral_notification_during_registration_does_not_poison_connection`; lanes-12 pending.

**Target state.** Lane green; a property test that any server-produced unilateral PDU (from the codec's notification set) arriving in either handshake phase is stashed, never poisons, and is delivered after `Ready`.

**Acceptance.** Positive: lane + proptest green. Planted negative: a *correlated* reply with a wrong PDU type during registration still fails with `InboundPduInvalidForPhase`. **Effort.** S. **Beads.** ft-xxfwy.34.

### G74 / G77 — Writer never drops a segment; cursor re-converges

**Status now.** Group commits `BEGIN IMMEDIATE` (7a30c6560; nine test expectations updated to the new statement text; local writer tests green after the update, remote lane 13 pending); the capture-side counter realignment is not done; snapshot-engine startup warnings share the root.

**Target state.** Under a concurrent writer, `append_segment` waits (busy_timeout) instead of failing; if a persist still fails, the capture loop retries with backoff and does **not** advance its sequence; after any discontinuity the cursor and the capture counter converge so the warning fires at most once; the smoke reports `dropped segments: 0`, `sequence resyncs: 0` on ten consecutive runs.

**Design.** (1) Audit every `run_writer_transaction(…, Deferred, …)` caller for read-then-write shape; make those `Immediate` (done for the two group commits; check `snapshot_engine` and retention cleanup paths). (2) In `runtime.rs` capture loop: on `Err` from `persist_captured_segment_for_runtime`, classify `busy|locked` as retryable, retry up to N with jittered backoff, and only then drop with a `capture.persist.dropped` metric; keep `captured_seq` = last persisted + 1 by reading it back from the cursor after resync (`cursor.resync_seq` already updates the cursor; the segment builder must source its `seq` from the cursor, not a private counter). (3) A test with two connections: one holds a write transaction for 200 ms while the other appends → append succeeds after the wait; a fault-injected persist failure → one warning, converged cursor.

**Acceptance.** Positive: both tests green on a lane; smoke counters zero. Planted negative: with the busy timeout set to 0 the append fails and the retry path is exercised (assert the metric). No-claim: does not address multi-process writers beyond SQLite's WAL semantics. **Effort.** M. **Beads.** ft-xxfwy.32.

### G75 — Mux-server operability: logging and config honesty

**Target state.** `frankenterm-mux-server` installs the same `env_bootstrap` logger as the GUI (`RUST_LOG`/`WEZTERM_LOG` honored), logs the resolved config path and every unix domain socket it binds at `info`, and exits non-zero with the error when `--config-file` cannot be loaded. **Tests.** Unit: a config with a syntax error → `run()` returns `Err` naming the file; integration: start with a temp config whose `unix_domains[0].socket_path` is under a temp dir and assert the socket appears there. **Acceptance.** Positive: the smoke can isolate its socket (drop the "always binds RUNTIME_DIR/sock" caveat). Planted negative: unreadable config → exit 1. **Effort.** S. **Beads.** ft-xxfwy.35.

### G76 — `ft send` paste-mode contract

**Target state.** `ft send --help`, `docs/robot-contracts/send.md`, and the README state that the default is bracketed paste (right for agent TUIs) and that `--no-paste` types text (right for shells); `ft robot send` gains `paste: bool` in its receipt so callers can see which mode ran. **Tests.** Contract test asserting the receipt field; docs stamp check. **Effort.** S. **Beads.** ft-xxfwy.36.

### G78 — Four tracked orphan source files

Owner decision per file (delete, or declare with a real consumer); KNOWN_ORPHANS shrinks in the same commit; guard lane green. **Beads.** ft-xxfwy.31.

### 4b. Cross-cutting rules that every bead above inherits

1. **Generation binding.** Every receipt records `commit`, `cli_version`, `mux_version`/`app_version`, `codec_version`, `host`, and the worker for remote lanes. A receipt from a different generation than the release under test is not evidence for that release.
2. **No silent pass.** A skipped or informational assertion can never produce `status: pass`; the receipt schema is validated by `scripts/attestation-build.sh`.
3. **Proof lanes.** Remote, fail-closed (`RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1`), `TMPDIR=/tmp`, no worker pins, one core-class lane at a time, the log retained and cited by hash in the bead comment.
4. **Degradation.** Every operator surface added (doctor rows, envelope hints) must say what it could not determine and why, never guess.
5. **Docs follow code in the same commit** when a claim changes (README, `docs/robot-contracts/`, `docs_gen.rs` descriptions).
6. **Removal needs authorization** (AGENTS.md Rule 1): beads .16, .25, .31 record the owner's words before any file is deleted.

### 4c. Critical path and sequencing

1. Proof of today's fixes (lanes 11–13) → close .5/.22/.25 if green; keep .7/.18/.32/.33/.34 open until green.
2. Operator step: `dsr release` v0.15.2 with signing (.1/.2) — nothing else unblocks the entry ramp for users.
3. .3/.4 idle-host activation e2e, .8 attach e2e against the v0.15.2 app, .12/.13 shell integration.
4. .9 tier-1 live-loop proof (needs .8, .12), then .11 dogfood, .15 signed bundle + DSR gates (.16), .17 suite status.
5. .10 tiers 2/3, .20 headline claims, .23 backports, .27 rule provenance, .28 sweep, .14 kill-switch persistence.
6. .26 main.rs split, .29 formal lanes, .21 demo, .24 remaining health rules, .35/.36 operability contracts, .31 orphans.
7. .30 acceptance: README Quick Install + 10-Minute Tour verbatim on clean hosts.

---

## 5. Would completing all open beads close the vision gap?

After this revision, yes for the entry ramp and the truthfulness of the README, provided the beads are closed on their stated evidence (receipts, lanes, signed slots) and not on inspection. Not yet for scale (V13/V14/V18), which stays with the July programs and G58's real rig. The correct sequencing is unchanged: entry ramp first (G50, G51, G52, G53, G54, G71, G72), then truthfulness (G55–G61, G74–G76), then cadence and structure (G62–G70, G78), while the scale programs continue at demoted priority.

---

## 6. Program-health findings (beads)

| Signal | 2026-09-01 | 2026-09-02 |
|---|---|---|
| Open + in_progress + blocked + deferred | 940 | ~940 (+6 new children, several closeable) |
| Open P0 | 434 (252 in `7xqz4`, 167 in `4tenz`) | 25 |
| in_progress with no update > 14 days | 193 of 230 | 71 released, 26 broadcast |
| Created / closed per month | Jun 419/333 · Jul 382/117 · Aug 414/68 | trend not yet reversed |
| Commits per month | Jun 1,016 · Jul 240 · Aug 1,742 | — |
| `ft-tf6g3` children | 75 closed / 5 blocked | unchanged |
| `ft-7h5da` W0–W12 | 60 closed / 11 in_progress / 37 open / 36 blocked | unchanged |
| `7xqz4` (2026-07-27) | 4 closed / 44 in_progress / 237 open / 1 blocked | demoted to P2 |
| `4tenz` | 43 closed / 77 in_progress / 257 open | demoted to P2 |
| Cycles (active scope) | 0 | 0 |

---

## 7. Bead epic and children (`br` only)

**Epic:** `ft-xxfwy` — `[reality-check-2026-09-01] Entry-ramp truth epic`. Labels: `reality-check`, `reality-check-2026-09-01`. Every child carries a `proof_category:` line and, after the 2026-09-02 revision, the full §4 section for its gap appended under "Phase 2 closure plan" so the bead is self-contained.

| Bead | Gap | Type / P | Title (short) | Depends on | State 2026-09-02 |
|---|---|---|---|---|---|
| ft-xxfwy.1 | G50 | task P0 | Sign DSR release artifacts; manifest `signed:true`; verify fails on unsigned | — | verify script done; signing hook open |
| ft-xxfwy.2 | G50 | task P0 | Publish signed v0.15.2 + clean-host installer e2e | .1 | open (operator) |
| ft-xxfwy.3 | G50 | feature P0 | Installer activation on idle hosts; `--activate`; live-host next_action | — | `--activate` landed; auto-if-idle open |
| ft-xxfwy.4 | G50 | test P0 | Installer activation e2e (idle, live hold, failpoint rollback) | .3 | open |
| ft-xxfwy.5 | G51 | feature P0 | Ranked mux socket discovery incl. GUI default-class symlink | — | landed; lane green → closeable |
| ft-xxfwy.6 | G51 | test P0 | Discovery unit/proptest/differential/native tests | .5 | unit done; differential/native open |
| ft-xxfwy.7 | G51/G66 | feature P1 | Typed `robot.mux_version_skew` + doctor pairing + installer guidance | .5 | code landed; installer half + lane open |
| ft-xxfwy.8 | G51 | test P0 | Native macOS e2e: `ft` attaches to running FrankenTerm.app | .5, .7, .33 | headless variant passes; app variant open |
| ft-xxfwy.9 | G52 | task P0 | Live-loop proof tier 1 (3 real agent panes) | .8, .12 | precursor smoke passes |
| ft-xxfwy.10 | G52 | task P1 | Live-loop tiers 2/3 with authoritative gates | .9 | open |
| ft-xxfwy.11 | G53 | task P1 | Dogfood status gate + launchd template + attestation slot | .8 | script done |
| ft-xxfwy.12 | G55 | feature P1 | Prompt-active evidence everywhere; `ft setup shell-integration` | — | CLI half landed |
| ft-xxfwy.13 | G55 | test P1 | Prompt-capability proptest + two-pane e2e + tx precondition e2e | .12 | open |
| ft-xxfwy.14 | G56 | task P1 | Kill-switch tier proof + persisted state + conformance artifact | — | doctor honesty landed |
| ft-xxfwy.15 | G54 | task P0 | First real signed attestation bundle + DSR verifier wiring | .1, .17 | open |
| ft-xxfwy.16 | G54 | task P2 | Retire `.github/workflows` (done) + DSR wiring of `release-gates.sh` | .15 | retirement done; wiring open |
| ft-xxfwy.17 | G59 | task P1 | Test-suite status artifact; fix ft-ziprn; virtual-time retry test | — | baseline done |
| ft-xxfwy.18 | G57 | feature P2 | `/stream/events` live: storage tail (done) + `ft watch --web` | — | tail landed |
| ft-xxfwy.19 | G57 | test P2 | SSE live-events e2e | .18 | open |
| ft-xxfwy.20 | G58 | task P1 | Measured headline claims per release; real-pipeline rig | .9 | open |
| ft-xxfwy.21 | G60 | task P2 | Demo recording + README asset check | .8 | embeds removed |
| ft-xxfwy.22 | G61 | docs P2 | Documentation truth sweep | — | done → closeable |
| ft-xxfwy.23 | G62 | task P1 | Restore weekly upstream backport cadence + provenance batches | — | open |
| ft-xxfwy.24 | G63 | task P1 | Program-health reset (P0 ≤ 25 done; vertical-slice rule, weekly audit) | — | partial |
| ft-xxfwy.25 | G64 | task P3 | Remove orphaned `mcp_helpers.rs` + guard test | — | done → closeable on green guard lane |
| ft-xxfwy.26 | G65 | task P2 | Split `main.rs` isomorphically | — | open |
| ft-xxfwy.27 | G67/G73 | task P2 | Rule-pack currency + real-binary fixture provenance | — | codex casing fixed |
| ft-xxfwy.28 | G68/69 | task P2 | Verification sweep of unverified surfaces (+ `get-text` tail) | — | profile create done |
| ft-xxfwy.29 | G70 | task P3 | Formal-method lanes in DSR quality | — | open |
| ft-xxfwy.30 | final | test P0 | README Quick Install + 10-Minute Tour verbatim on clean hosts | .2, .3, .5, .12, .15, .22 | open |
| ft-xxfwy.31 | G78 | task P2 | Four tracked orphan files: owner authorization + baseline shrink | — | baselined |
| ft-xxfwy.32 | G74/G77 | bug P1 | Writer segment drop + cursor re-convergence | — | IMMEDIATE landed; realign open |
| ft-xxfwy.33 | G71 | task P0 | Listener abort fix: remote proof, smoke as DSR check, ship in v0.15.2 | — | fix landed |
| ft-xxfwy.34 | G72 | task P1 | Handshake unilateral-PDU fix: remote proof + proptest | — | fix landed |
| ft-xxfwy.35 | G75 | task P2 | Mux-server logger + `--config-file` honesty | — | open |
| ft-xxfwy.36 | G76 | docs P3 | `ft send` paste-mode contract + receipt field | — | open |

Existing beads this epic unblocks or subsumes (not re-parented): `ft-zhwa6` (via .12), `ft-l59nq` (via .14), `ft-zeo5o` (via .18), `ft-nam3s` + `ft-ziprn` (via .17), `ft-tf6g3.1` + `ft-e87u6` (via .15), `ft-d0ez0.5` (via .10), `ft-7h5da.10.3.2` + `ft-7h5da.10.4.3` (via .20), `ft-xl2kc.1` (via .21), `7xqz4.12` (referenced by .3, live-host activation).

---

## 8. Ambition and refinement record

- **Phase 1 (assessment, 2026-09-01):** evidence gathered from AGENTS.md/README (fully read), the two predecessor plans, `br`/`bv`/JSONL, live probes of the installed 0.13.0 and 0.15.1 binaries against the running 0.13.0 GUI, the v0.15.1 release manifest, the DSR repo configuration, install.sh activation and signature paths, and the code paths in `runtime.rs`, `workflows/runner.rs`, `tx_execution.rs`, `policy.rs`, `vendored.rs`, `web.rs`, `main.rs`.
- **Phase 2 (bridge plan):** the 2026-09-01 version of §4 was one paragraph per gap — a summary, not a closure plan. The 2026-09-02 revision (this file) is the Phase 2 deep pass: every gap G50–G78 now carries status, target, design, work breakdown, tests with artifact paths, acceptance with a planted negative and a no-claim line, dependencies, risks, effort, and beads; §4b/§4c add the inherited rules and the critical path.
- **Phase 3a (beads):** 30 children on 2026-09-01; 36 after this revision; every child's description carries its §4 section verbatim so the plan never needs to be consulted.
- **Phase 4 (ambition rounds):** one light round happened on 2026-09-01 (entry-ramp framing, acceptance gate .30, program-health reset .24). The "MUCH better in every way" rounds have **not** been run; they remain to be driven against this revision.
- **Phase 5 (refinement passes):** four light passes on 2026-09-01 (test companions, operator surfaces, authorization requirements, subsumption list). The frozen refinement prompt has not been run against the deep plan; do so after the ambition rounds, and expect each pass to add gaps.
- Graph audit: `br dep cycles --json` → 0 active cycles after wiring (re-checked after the 2026-09-02 additions; see §9).

### Robot-suggest hygiene checkpoint (AGENTS.md footer)

```text
bv --robot-suggest generated_at=2026-09-01T19:25:25Z data_hash=4898ef5201379b7e
suggestions: total=50 missing_dependency=20 potential_duplicate=20 label_suggestion=10 high_confidence=50 actionable=50
br dep cycles --json: count=0; active_ft_cycles=no
br sync --flush-only --json: errors=0 success_rate=1.0
```

Only one suggestion touched this epic: `ft-xxfwy.17` "may depend on ft-nam3s" (0.95, keyword overlap). Classified `already_implied` — .17 subsumes ft-nam3s and the honest edge direction is the reverse (ft-nam3s closes when .17 lands); no mutation applied. The other 49 suggestions concern legacy `wa-*` beads and pre-existing `ft-*` families and were not evaluated in this run (worksheet retained for the program-health bead .24).

## 9. Verification evidence recorded during this run

- `scripts/check-reality-check-due.sh --json` → due on 4 of 5 triggers (see header).
- Live probes (2026-09-01, maintainer Mac, FrankenTerm.app 0.13.0 pid 84278 running): `ft` 0.13.0 → `robot.wezterm_error` via the external `wezterm` CLI; `ft` 0.15.1 → `backend_failure` / `mux_transport_or_protocol_failure`; `ft doctor` on both → connection error rows.
- `gh release view v0.15.1` assets: 13 files, 0 `.minisig`; manifest artifacts all `signed:false`.
- Remote test lane launched during this run: the `frankenterm-core` library suite with three jobs on RCH worker hz2 (`CARGO_TARGET_DIR=/tmp/ft-rc20260901-core-lib`, `TMPDIR=/tmp`).

**Test-lane result (remote, hz2, 2026-09-01T19:0x–19:3x Z):** `frankenterm-core --lib` → 31,256 passed / 4 failed / 2 ignored, 130.68 s. Failures: `api_schema::tests::current_version_parses`, `snapshot_engine::tests::intelligent_exact_threshold_boundary`, `snapshot_engine::tests::periodic_mode_ignores_triggers`, `snapshot_engine::tests::run_periodic_with_cx_mid_flight_cancel_exits_quickly`. All four fixed the same day (f494eb398 and the snapshot timing commits).

**Lane 8 (remote, hz3, 2026-09-02T00:22–01:10 Z, final tree fdd7e0c07):** filtered core-lib set → 1000 passed / 1 failed (`retry::tests::retry_runs_deterministically_under_labruntime_with_cx`, 5.4 s wall on a loaded worker; passes locally in 0.19 s); `no_orphan_source_files` → failed on four pre-existing orphans (now baselined); `cargo check -p frankenterm --bin ft` → pass (hz4).

**Lanes 11–12 (remote, hz4, 2026-09-02T01:51–03:16 Z):** `retry::tests` + `patterns::tests::detect_codex_usage*` → 103 passed / 0 failed (the lane-8 retry failure was worker load); `no_orphan_source_files` → pass with the `KNOWN_ORPHANS` baseline; `frankenterm-mux-server-impl local::tests` → 7 / 0 (listener abort regression test included); `vendored::mux_client::tests` → 170 / 0 (unilateral-handshake regression test included). **Lane 14 (remote, hz4, 2026-09-02T03:19–03:20 Z, HEAD 75689f18a):** `storage::writer_io_scheduler_tests` + `storage::writer_epoch_transaction_tests` after the `BEGIN IMMEDIATE` change → 48 passed / 0 failed. (Lane 13 was launched before its script was corrected and matched zero tests; it is not evidence.)

**Dev signals (maintainer Mac, not proof):** `scripts/smoke/headless-mux-observe.sh /tmp/ft-rc20260901-native/debug` → PASS (event `codex.usage.reached`, `reset_time: 3:00 PM`, dropped segments 0, sequence resyncs 0 on the second run); local core tests: the new unilateral-handshake test, the codex casing test, the retry test, and the 48 writer-scheduler/epoch tests green after the `BEGIN IMMEDIATE` expectation update.

## 11. Execution log — same day (2026-09-01/02)

Work started on the entry ramp immediately after the plan was published. Everything below is committed on `main`; "proof" means a retained remote RCH lane transcript, "native" means a dev build run on the maintainer's Mac (a signal, not proof).

| Gap | Landed (commit) | Proof state |
|---|---|---|
| G51 socket discovery | `config::gui_socket` shared naming; ranked `discover_mux_socket_ranked` + `MuxSocketSource`; client/GUI re-exports; `build_unified_client` dials the discovered path; doctor `mux socket` row (f862716ce, 002df4c37, 3ae637088, b02802df5, 82b91e9b4, cbb0a396f) | client discovery 10 pass, core `wezterm::tests` 197 pass, config `gui_socket` 4 pass (hz2); lane 8 1000/1; native doctor shows `source: gui_published` against the running app |
| G51/G66 version skew | `WeztermError::VersionSkew`, `robot.mux_version_skew`, FT-1025, doctor pairing recommendation (0f64ca942, b02802df5) | lane 8 (wezterm/error/retry/sharding filters) |
| G55 prompt evidence (CLI half) | five CLI tx sites resolve pane capabilities (`resolve_tx_contract_capabilities`) | `cargo check -p frankenterm --bin ft` pass (hz2, hz4) |
| G57 web SSE | storage → bus tail (`StorageEventTail`) in `ft web`; default on (a950faf99, 744e52729) | lane 8 (`web::` filter) |
| G50 signing/activation | `scripts/release/verify-release.sh` (exits 1 on v0.15.1); `install.sh --activate <gen> --idle-host-confirmed` (da3b0e1fd) | verifier exercised against the real release; activation refusal paths smoke-tested; end-to-end activation NOT tested (ft-xxfwy.4) |
| G53 dogfood | `scripts/dogfood-status.sh` (83e55a72c) | run: stale, newest capture 4793 h |
| G54 gates | 25 workflow files retired; `scripts/release-gates.sh` (static + `--cargo`); tests/artifacts repointed; attestation gate builds then verifies the dev bundle | 28 of 28 static gates pass after fixes (bead-structure, runtime-proof census, coupling baseline); rerun of the three formerly failing gates 3/3 |
| G56 doctor honesty | policy rows labelled process-local (784bb8870); persistence gap recorded on ft-xxfwy.14 | kill-switch tests on lane 8 |
| G61 docs | README/AGENTS truth sweep (schema v45, `<version>.json`, DSR wording, detector, metrics row, Stateright wording, FAQ path, demo embeds removed) | `stamp-readme-counts.sh --check` pass |
| G63 program health | 606 P0/P1 → P2; 71 stale claims released; 26 broadcast; open P0 = 25 | — |
| G64 orphan | `mcp_helpers.rs` removed (authorized); `no_orphan_source_files` guard test; census re-blessed; four pre-existing orphans baselined (ft-xxfwy.31) | guard lane pending (lanes-11) |
| G68/G69 sweep | finding: no production path created agent profiles → `ft robot profile create` added (5945f1127; declaration fix 1dd2ea159) | handler tests on lanes-11 |
| test baseline | fresh core-lib run 31,256 pass / 4 fail → all four fixed | lane 8 |
| G71 listener abort | `admit_connection` / `handoff_to_main_thread_local` + regression test (647d87fd6) | lanes-11 pending; headless smoke PASS |
| G72 handshake race | unilateral PDUs pass the phase gate + scripted-server test (0f00b7cf5) | lanes-12 pending; local test green |
| G73 codex casing | `(?i)` regex + real-string test (e903495ee) | lanes-11 pending; local test green |
| G74 segment drop | group commits `BEGIN IMMEDIATE` + expectation updates (7a30c6560 + follow-up) | lanes-13 pending; local writer tests green |
| G52 precursor | `scripts/smoke/headless-mux-observe.sh` (4f94bc18b) | PASS on the dev build |

**Headless attach smoke (2026-09-02, dev build, same generation).** A dev `frankenterm-mux-server` plus the dev `ft` from the same build were run on the maintainer's Mac to get a same-codec attach without the 0.13.0 app. Result: the mux server aborted (SIGABRT) on the first client connect. Root cause: `LocalListener::run` (shared by the headless server and the GUI's published-socket listener) called `reservation.spawn_local` from the listener thread; async_task's thread check panicked on the main thread's first poll and again during drop, so the whole server died and the client only saw "codec_version_handshake: response read EOF". The promise crate documents this exact hazard and ships `handoff_to_main_thread_local`; the listener now uses it (`admit_connection`) with a regression test that aborted under the old code. This is the concrete mechanism behind G51 for same-generation pairs on HEAD; it is invisible against the 0.13.0 app because that build pre-dates the admission refactor.

With the fix in place the same smoke passed end to end (dev build, 2026-09-02 01:47 UTC): `ft doctor` reports the mux socket (source: environment) and "1 pane(s) detected via vendored client"; `ft list` returns pane 0; `ft watch --foreground` opens a vendored pane streaming subscription; `ft send --no-paste` sets the pane title to `codex` and echoes the Codex usage-limit message; `ft events` returns event 1, rule `codex.usage.reached`, severity critical, `reset_time = "3:00 PM"`. That is a precursor of tier 1 of the live loop (§4 G52) on a real mux, minus agents, minus auto-handle, minus a release artifact.

Follow-on defects fixed from the same smoke: the real Codex binary prints " Try again at " with a capital T (G73); a pane spawning during the handshake makes the mux broadcast a unilateral `TabResized` before the registration reply (G72); the writer dropped a segment under a deferred read-then-write transaction (G74). Findings recorded, not fixed: `frankenterm-mux-server --config-file` is silently ignored because no logger is installed (G75); sends default to bracketed paste which zsh does not execute (G76); four more tracked orphan files (G78); `ft get-text` returned blank lines for a pane with output (G69).

**First CLI ↔ GUI attach (2026-09-02 04:07 UTC, dev generation).** `frankenterm-gui` was built from HEAD on the Mac (8 min 21 s) and launched as a second window class (`--class com.dicklesworthstone.frankenterm.rc`, `--skip-config`, `--always-new-process`) so the daily 0.13.0 app and its `default-<bundle>` symlink stayed untouched. It published `frankenterm-gui-sock-<pid>`; with `WEZTERM_UNIX_SOCKET` pointing at it, `ft doctor` reported the socket (source: environment) and "1 pane(s) detected via vendored client", and `ft list --json` returned the GUI's pane. That is the first time any `ft` has listed a running FrankenTerm GUI's panes. Caveats: dev binaries, explicit socket via env (the published-symlink rank was proven separately and would have chosen the skewed 0.13.0 app here), a class-scoped instance.

**Later the same day (04:00–04:15 UTC).** The dev binaries were rebuilt from HEAD plus the working tree and the receipt-emitting smoke passed 7/7 steps (`dropped segments 0, sequence resyncs 0`); a forced-contention experiment (a second connection holding `BEGIN IMMEDIATE` for 8 s across watcher start) produced no drop and contiguous sequences. The mux server now installs a logger (`env_logger`, default `info`) and prints its config source and every bound socket; the logger immediately explained G75: this fork loads only `frankenterm.toml` unless `FRANKENTERM_LUA_CONFIG=1`, so an explicit `.lua` file was skipped without an error. The server now refuses to start when an explicit config file is not a TOML file while Lua is disabled, or when the explicit file records a load error (tested). The observe loop through the GUI pane captured output fine but produced no event because the GUI reported the pane title as the last command name (`exec`) rather than the OSC-2 title (recorded on ft-xxfwy.28).

**Two more findings from the live SSE e2e (04:30–05:15 UTC).** (G79, ft-xxfwy.37, fixed 0d3378eac) The mux server aborted at teardown: a waker firing from the uds fallback-rewake thread after the executor generation retired made `enqueue_admitted` drop a `spawn_local` runnable on that thread; the queue now records its owner thread and leaks off-thread retired runnables instead (counted), with a regression test. (G80, ft-xxfwy.38, P0, open) `ft web` on HEAD accepts TCP connections but answers no request at all: `/bookmarks`, `/`, and `/stream/events` all time out with no status line, while the installed 0.13.0 answers `/bookmarks` in 75 ms. An A/B with the new `FT_WEB_STORAGE_TAIL=0` knob shows the storage tail is not the cause; `sample` puts the main thread inside fastapi-rust's `accept_loop_app_concurrent`, in the 50 ms `timeout` around `TcpListener::accept`, looking up a timer driver through `Cx::current`. The first theory (a driverless task context) was wrong: an in-process test passed with a fix built on it while the binary still hung, and that change was reverted. The confirmed cause (06:20 UTC): fastapi-http's `current_time()` measured time from a private start instant initialized at its first call, while asupersync's `timeout` compares deadlines against the runtime's process-epoch clock (`wall_now`) *before* polling the inner future; in `ft web` the runtime starts seconds before the server (storage opens first), so every 50 ms accept timeout, and every keep-alive read deadline, is born already expired and `accept` is never polled. Kernel view: the client connection sits ESTABLISHED in the listen backlog with the request bytes queued and the process holds no accepted socket. The fix is one line in fastapi_rust (`current_time()` returns `asupersync::time::wall_now()`, plus a clock-skew regression test) and a rev bump here; `web_framework::tests::server_started_after_runtime_clock_skew_answers_requests` reproduces the hang by waiting 250 ms between building the runtime and starting the server. The SSE e2e (`tests/e2e/test_web_sse_live_events.sh`) is the acceptance test for ft-xxfwy.19. Lanes 15/17 lost two jobs to rch `sync_to_remote` timeouts (424 s, twice) — fleet noise, reruns queued.

Not done today: signed release (needs an operator-driven `dsr build/release` with signing on), the attach e2e against a shipped app bundle, live-loop tier 1 with agents, `ft setup shell-integration`, the kill-switch surface and persistence (the engine's kill switch turned out to have no production caller at all), the capture-counter realignment.

**G80 root cause and fix (2026-09-02 06:00–08:40 UTC, ft-xxfwy.38).** `ft web` bound its port and never answered because fastapi-http's `TcpServer::current_time()` measured time from a private `START_TIME` claimed at its first call, while asupersync's `timeout()` judges the deadline against the runtime's process-epoch clock *before* polling the inner future; once process uptime passed the 50 ms accept-poll interval every accept timeout was born expired and `accept()` was never polled (netstat: peer ESTABLISHED in the backlog, Recv-Q > 0, no accepted fd). An earlier theory (driverless request Cx) passed in-process and did not fix the binary; it was reverted. The fix is upstream: fastapi_rust branch `fix/http-clock-process-epoch-0.3` @ 1718091b (`current_time()` = `asupersync::time::wall_now()`, regression test `current_time_shares_asupersync_process_epoch`; lib suite 426/0/1-ignored — the ignored `serve_concurrent_shutdown_wakes_idle_accept_loop` only ever passed because of the expired-deadline hot loop, since asupersync 0.3.x's `block_on` never fires the crate's timeouts; `tests/http2.rs` failures reproduce on the pristine pin). frankenterm pins that rev (Cargo.toml + eight lockfile entries, nothing else moved). Proof: `web_framework::tests::server_started_after_runtime_clock_skew_answers_requests` failed before the pin ("" after 5 s) and passed after it locally and on remote lane 20 (13 passed, worker vmi1153651); the rebuilt dev `ft web` answers `/health`, `/bookmarks`, `/panes` with 200 and `/nope` with 404. The same test is now the `--cargo` release gate "web api liveness after clock skew".

**G80 second layer (same run).** With accept working, the first `/stream/events` client aborted the whole `ft web` process: the SSE handler runs on a connection task that fastapi-http spawns itself, on a worker thread where the wrapper's `ASUPERSYNC_HANDLE` thread-local is empty, so `task::spawn_with_cx` panicked ("called outside of Runtime::block_on context"). `runtime_async::task::{spawn, spawn_with_cx, try_spawn_with_cx}` now fall back to asupersync's own `Runtime::current_handle()` for the task being polled (`scheduler_runtime_handle`), and `web_framework::tests::handler_on_a_fastapi_connection_task_can_spawn_a_child_task` reproduces the mechanism. The SSE e2e result against the rebuilt binaries is recorded in ft-xxfwy.19.

**G56 kill switch (2026-09-02, ft-xxfwy.14, closes ft-l59nq).** The June tier gate (f8c674376) was correct and unreachable: the watcher's auto-handler used `PolicyEngine::permissive()`, every CLI action built a fresh engine, and nothing in production ever called `trip_kill_switch`. Remote lane 21 proved the gate itself (8/8 `killswitch_*` + connector typed-code tests, worker hz4) and ft-l59nq closed on that evidence. The production trigger now exists: `policy_kill_switch_state` persists the operator kill switch in the workspace DB `config` row `policy.kill_switch_v1` (missing = disarmed; unreadable/corrupt = HardStop, fail closed, reported as `failed_closed`; restore never re-audits), `ft robot kill-switch status|trip|reset` writes it through the audited `PolicyEngine::trip_kill_switch`, `with_persisted_kill_switch` restores it into the watcher engine and 15 CLI action engines (connector via its one-shot backend), and `ft doctor`'s policy rows read the persisted tier without creating a DB. Dev smoke on the rebuilt binary: disarmed → trip hard-stop → status hard_stop → trip soft-stop refused (`robot.kill_switch.trip_rejected`) → reset → trip soft-stop → doctor row "persisted operator kill switch: soft_stop (by operator, reason: after-reset, …)". Contract: docs/robot-contracts/kill-switch.md; conformance artifact `docs/attestations/proofs/killswitch-tier-enforcement.json` (4 tiers × 24 ActionKinds × pane/no-pane) generated and drift-checked by `crates/frankenterm-core/tests/killswitch_tier_matrix.rs` and registered in the attestation manifest. Not wired: `prepare send` and the prepare-workflow send-summary site (dry-run planners without a storage handle).

## 10. Successor note

The next full reality-check must cross-link this plan, `docs/reality-check-bridge-plan-2026-05-12.md`, and `docs/reality-check-bridge-plan.md`, and must start from `ft-xxfwy`'s terminal state. It becomes due per `docs/process/reality-check-discipline.md` (90 days, minor-version change, ≥ 50 open beads, contract churn, or headline-claim growth). Do not overwrite this file; revise it in place only within this run.
