# FrankenTerm Reality-Check Bridge Plan — 2026-09-01

**Source:** `/reality-check-for-project` invocation 2026-09-01 (third full run). Read AGENTS.md (1,747 lines) and README.md (4,454 lines) in full, then audited HEAD `f3397177e` (workspace version `0.15.2-rc.1`) against the promises in those two documents.

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

### The five questions

**1. What specifically IS working right now (code-verified, tests exist)?**
- Watcher runtime: discovery → 4 KB overlap delta → gap events → FTS5 persistence → pattern detection (Bloom + Aho-Corasick + fancy_regex, 94 rules across 4 agent families with 251 corpus fixtures) → event bus → workflow runner. `runtime.rs` wires native push events, BOCPD, backpressure tiers, fleet memory, and the recorder. The unclean-session detector runs at `ft watch` start (`main.rs:42598`; README under-claims this).
- Workflow reaction path: `StepResult::SendText` is executed through the policy-gated injector with `ActorKind::Workflow` (`workflows/runner.rs:2451-2470`); `--auto-handle` builds the runner (`main.rs:42847`). Handlers for compaction, usage limits, Claude/Gemini limits, auth-required, session start/end, process triage, cass search, swarm learning all exist and several emit real send steps.
- Tx engine: `execute_step_action` sends through the mux (`tx_execution.rs:2447-2518`); prepare phase uses `PolicyPrepareStepExecutor` wired to the real policy engine; TLA+ kill-switch spec model-checked (258 states, 0 violations).
- Robot Mode: 50 command families, zero live `robot.not_implemented` routes, honest fail-closed sites for snapshot restore / restart / checkpoint rollback. Verified-submit re-reads the pane after send. Prometheus `/metrics` server exists behind the undocumented `metrics` feature.
- GUI: FrankenTerm.app 0.13.0 runs continuously on the maintainer's Mac with SSH-tunnelled remote mux domains. Windows, Linux, macOS CLI + app assets were published for v0.15.1.
- Session persistence is honestly scoped: every non-dry restore/rollback/restart fails closed exactly as the README says.

**2. What is NOT working or not implemented?**
- **Install:** the v0.15.1 release manifest marks every artifact `"signed": false` and no `.minisig` asset exists, while `install.sh` exits 1 when minisign verification fails unless `--no-verify`. Even on success the process family is left `activation: pending` (no `ft` on PATH) because the cross-launcher lease transaction is not implemented. The documented Quick Install cannot produce a working `ft` today.
- **CLI ↔ GUI attach:** `frankenterm-core::vendored::discover_canonical_mux_socket` only reads config `unix_domains` and `RUNTIME_DIR/sock`; the GUI publishes `frankenterm-gui-sock-<pid>` plus a `default-<bundle-id>` symlink through `client::discovery`, which core never references. Live probes: 0.15.1 CLI vs running 0.13.0 GUI → `backend_failure` / `mux_transport_or_protocol_failure`; 0.13.0 CLI falls back to the external `wezterm` binary and cannot decode FrankenTerm PDUs. No installed `ft` on this machine can list the running app's panes.
- **Live proof of the loop:** `ft-d0ez0` (P0, open since April) closed its 3/20/50-pane children on a fake WezTerm adapter; `ft-d0ez0.5` records that the 50-pane assertions were unconditional (false-green). The observe → detect → react → audit loop has never been run on live agents with retained artifacts.
- **Dogfooding:** the only `ft.db` on this machine is the repo workspace one: 863 segments, 1 event, last capture **2026-02-14**, stale `watch.lock` from 2026-08-06.
- **Attestation moat:** only `docs/attestations/0.0.0-dev.json` (`method: unsigned`) exists; no bundle for v0.13/v0.14/v0.15.x; README tells users to verify `docs/attestations/0.2.0.json`, which does not exist. `ft-tf6g3.1` (first real bundle) is blocked; `ft-e87u6` has had no activity for 78 days. The DSR quality checks run check/clippy/test/format but not `attestation-verify.sh`, so the README sentence "Release CI runs the shell verifier with --strict-deferred" is not true under the DSR-only rule.
- **Perf evidence:** `docs/perf/releases/` holds only a README; the swarm-capacity envelope is `blocked_target_class_not_proven`; the target-class cockpit is `skipped_not_proven`; the load rig emits synthetic metrics (`ft-7h5da.10.3.2`) and the cockpit script runs no bench lane (`ft-7h5da.10.4.3`).
- **Security items still open:** kill-switch SoftStop/HardStop tiers fail open for workflow/connector/file/exec actions (`ft-l59nq`, P1, fix committed 2026-06-15, never proven green); `ft tx run` / `ft tx rollback` build prepare lookups with unknown pane capabilities so PromptActive preconditions can never pass from the CLI (`ft-zhwa6`).
- **Test suite:** last full `frankenterm-core --lib` baseline is 2026-07-26 (29,553 pass / 85 fail, `ft-nam3s`); no retained August result; `ft-ziprn` TMPDIR defect still open; RCH reports a 58 % fleet-wide test pass rate. A fresh remote lane was launched during this run (see §9).
- **Marketing artifacts:** README embeds `assets/demo.gif` and `assets/demo-full.gif`; neither file exists (tapes do). `ft-jjvxg`/`ft-xl2kc` are closed anyway.
- **Process drift:** last upstream WezTerm backport commit is 2026-05-13 (weekly cadence promised); 25 GitHub workflow files (including `release.yml`, `ci.yml` edited 2026-08-31) coexist with Rule 0.1 "GitHub Actions must never be used"; `mcp_helpers.rs` (49 KB, orphaned) is still on disk after `ft-nfk94` closed.

**3. What is blocking us?**
- The cross-launcher lifetime / PTY-guardian handoff transaction (activation) is designed but not built; every install stays pending by design.
- Signing was never wired into the DSR release path, so verification-required installers reject DSR output.
- RCH admission for core-class proof lanes is scarce (memory: solo runs only, 3-slot workers, git-fetch stalls), which is why many "proof" beads sit blocked for months.
- Priority inflation: 434 open P0 beads means P0 no longer sequences anything; the two 2026-07-27 mega-epics (`7xqz4`: 286 children, 4 closed; `4tenz`: 377 children, 43 closed) absorb attention while first-run truth regressed.
- 193 of 230 in_progress beads have had no update for more than 14 days (abandoned claims).

**4. If every open and in-progress bead were implemented, would the gap close?**
Only partially. The `7xqz4` program would, in principle, deliver atomic install, journeys, continuity and release qualification, and `ft-d0ez0` would deliver the live proof. But five concrete defects found today have **no bead**: unsigned v0.15.1 artifacts versus a signature-required installer; core socket discovery ignoring the GUI-published default socket; the missing demo GIFs; the stalled upstream-backport cadence; and the orphaned `mcp_helpers.rs`. Several others are only covered by stale in_progress/blocked beads that will not move without an unblocking plan (`ft-l59nq`, `ft-zeo5o`, `ft-tf6g3.1`, `ft-e87u6`, `ft-nam3s`). And the bead graph itself is now a blocker (see G63).

**5. Vision goals with zero bead coverage:** G50 (unsigned release vs installer), G51 (GUI socket discovery from core), G53 (dogfooding cadence), G60 (demo assets), G61 (doc-drift sweep), G62 (upstream backport cadence), G63 (program health), G64 (orphan removal authorization), G65 (main.rs monolith), G67 (rule-pack currency cadence), G70 (formal-method packaging).

---

## 2. Vision checklist

Status vocabulary: WORKING / PARTIAL / STUB / UNPROVEN / NOT_STARTED / REGRESSED / NO_BEAD (added when no bead covers the goal).

| # | Goal (README/AGENTS promise) | Status | Severity | Bead coverage | Evidence |
|---|---|---|---|---|---|
| V1 | `curl \| bash` installs a runnable `ft` (+ app on Apple Silicon) | REGRESSED | Critical | `7xqz4.2` (broad); defect NO_BEAD | v0.15.1 manifest `signed:false`, 0 `.minisig` assets; `install.sh:4649-4653` exits 1; activation `pending` by design (`install.sh:1785-1789`) |
| V2 | `ft` attaches natively to the running FrankenTerm mux without external `wezterm` | PARTIAL / REGRESSED | Critical | NO_BEAD | `vendored.rs::discover_canonical_mux_socket` vs `client/src/discovery.rs:620`; live probes fail (§1) |
| V3 | Watch loop: discovery, delta, gaps, scheduling, push events, maintenance | WORKING (code), UNPROVEN (live) | Major | `ft-d0ez0` | `runtime.rs` refs: native_events 33, backpressure 115, bocpd 66, recorder 262 |
| V4 | Pattern packs for Codex / Claude Code / Gemini / runtime; lint; fixtures | WORKING | — | rule-drift cadence NO_BEAD | 94 rules, 251 fixtures, last fixture change 2026-06-08 |
| V5 | Workflows auto-handle events with policy-gated sends | WORKING (code), UNPROVEN (live) | Major | `ft-d0ez0`, `ft-l59nq` | `runner.rs:2451-2470`, `main.rs:42847` |
| V6 | Policy gate: prompt-active, alt-screen, rate limits, approvals, audit | WORKING; practical gap | Major | `ft-zhwa6` (open), `ft-g2ari` (closed) | `policy.rs:592` derives prompt_active from OSC-133 in storage; untrusted actor → RequireApproval (`policy.rs:7140+`) |
| V7 | Robot Mode 50 families, fail-closed where unbuilt | WORKING | — | — | `main.rs` RobotCommands; 0 live not_implemented routes |
| V8 | MCP server mirrors Robot Mode | WORKING (feature) | Minor | `ft-nfk94` closed | orphan `mcp_helpers.rs` still present |
| V9 | Web API + live SSE `/stream/events` | PARTIAL | Major | `ft-zeo5o` (in_progress, stale since 2026-06-15) | `web.rs:7-13` documents publisher-less bus |
| V10 | Distributed mode | WORKING (tests) | — | — | `distributed_streaming_e2e.rs`; not re-verified live |
| V11 | Lexical / semantic / hybrid search | WORKING (feature-gated); Tantivy path UNPROVEN | Minor | — | `main.rs:9474-9478`; tantivy optional dep |
| V12 | Mission + Tx prepare/commit/compensate on real panes | PARTIAL | Major | `ft-zhwa6` | `tx_execution.rs:2447-2518`; CLI capability gap |
| V13 | Operating envelope fail-closed admission | PARTIAL / UNPROVEN | Major | `ft-booek` (open) | artifact `blocked_rch_no_verdict`; target-class `skipped_not_proven` |
| V14 | Fleet memory + 3-tier scrollback + backpressure at 200 panes | PARTIAL (synthetic only) | Major | `ft-7h5da.10.*`, `ft-d0ez0.5` | load rig synthetic; false-green 50-pane test |
| V15 | Session persistence (honestly scoped restore-unavailable) | WORKING as documented | — | `7xqz4.8` | fail-closed sites `main.rs:26547-26594` |
| V16 | Incident bundles with live collectors | UNPROVEN | Minor | — | not verified this pass (G69 sweep) |
| V17 | Every headline claim → signed per-release attestation | REGRESSED | Critical | `ft-tf6g3.1` blocked, `ft-e87u6` stale | only `0.0.0-dev.json` unsigned; README cites nonexistent `0.2.0.json` |
| V18 | Perf targets (<50 ms capture, <10 ms FTS, 200-pane ~200 MB) | PARTIAL (hedged) | Major | `ft-7h5da.10.*` | no `docs/perf/releases/*.json`; synthetic rig |
| V19 | Formal methods (Loom, TLA+, Stateright, Lean) | WORKING (mostly) | Minor | `ft-tf6g3.*` closed | TLC pass retained; "Stateright" = hand-rolled models, no crate; Lean check not in any gate |
| V20 | Test suite green | UNPROVEN | Major | `ft-nam3s` in_progress, `ft-ziprn` open | last baseline 85 failures (2026-07-26) |
| V21 | Live observe→detect→react→audit proof at 3/20/50 panes | NOT PROVEN | Critical | `ft-d0ez0` open, `.5` in_progress | fake adapter + unconditional assertions |
| V22 | README demo recordings | REGRESSED | Minor | NO_BEAD (`ft-xl2kc.1` deferred) | `assets/demo*.gif` absent |
| V23 | Documentation accuracy | PARTIAL | Minor | NO_BEAD | schema v43 vs `SCHEMA_VERSION = 45`; `ft reality-check` verbs are `scripts/ft-reality-check.sh`; `metrics` feature undocumented |
| V24 | Weekly WezTerm upstream backports | REGRESSED | Major | NO_BEAD | last `Upstream-WezTerm` commit 2026-05-13 |
| V25 | DSR-only releases, no GitHub Actions | PARTIAL (doctrine contradiction) | Minor | — | 25 workflow files, `ci.yml` edited 2026-08-31 |
| V26 | Atomic process family + PTY guardian handoff + activation | PARTIAL | Critical | `7xqz4.2`, `7xqz4.12` | activation transaction unimplemented |
| V27 | Program health / bead graph as a truthful ready queue | REGRESSED | Major | NO_BEAD | 434 open P0; 193 stale in_progress; creation > closure since June |
| V28 | Windows support | PARTIAL | Minor | `ft-azsnz` | `ft-windows-amd64.zip` shipped |
| V29 | WASM extension runtime | NOT_STARTED (documented) | — | — | README honest |
| V30 | Maintainable codebase ("no tech debt") | PARTIAL | Major | NO_BEAD | `crates/frankenterm/src/main.rs` = 129,742 lines |

---

## 3. Gap register (G50–G70)

Each gap carries the `proof_category:` line required by `docs/proof-taxonomy.json` (IDs 1–11 or the non-proof slugs substrate / process / infrastructure / coordination).

### G50 — Latest release is not installable through the documented path — REGRESSED (Critical)
**Promise:** "The installer verifies the DSR-published checksum and minisign signature, then publishes `ft`…" (README Quick Install).
**Reality:** `frankenterm-v0.15.1-manifest.json` lists four artifacts with `"signed": false, "signature_file": ""`; the release has no `.minisig` asset; `install.sh` requires `minisign` and exits 1 on verification failure (`install.sh:3391-3396`, `4649-4653`). `--no-verify` is the only way through, and even then the process family stays `activation: pending` (no stable `ft`) until a transaction that does not exist runs.
**Bead coverage:** none for the unsigned-release defect; `7xqz4.2` covers atomic install broadly.
`proof_category: 10 (cryptographic-attestation) + process`

### G51 — `ft` cannot discover or attach to the running FrankenTerm.app — PARTIAL/REGRESSED (Critical)
**Promise:** "Native vendored builds prefer the direct pooled mux protocol; the external `wezterm` CLI is a compatibility fallback."
**Reality:** core resolves only config `unix_domains` or `RUNTIME_DIR/sock` (`vendored.rs`, `vendored/mux_client.rs:4666`). The GUI publishes `frankenterm-gui-sock-<pid>` and the `default-com.dicklesworthstone.frankenterm` symlink via `frankenterm/client/src/discovery.rs:620`, which core never calls. With the 0.13.0 app running, the 0.15.1 CLI returns `backend_failure` / `mux_transport_or_protocol_failure`; the 0.13.0 CLI falls back to the external `wezterm` and cannot decode PDUs. Version skew between CLI and GUI produces no actionable diagnostic.
**Bead coverage:** none.
`proof_category: 4 (conformance-artifact) + 9 (differential-comparison)`

### G52 — Core loop never proven on live agents; prior proofs were false-green — NOT PROVEN (Critical)
**Promise:** "observe, detect, react, audit" with "10 live agents, real detections, no mocks".
**Reality:** `ft-d0ez0.1-.3` closed on a fake WezTerm adapter; `ft-d0ez0.5` documents unconditional latency/pressure assertions; `ft-7h5da.10.3.2` documents a synthetic load rig. No retained artifact shows a real Claude Code / Codex pane hitting a rate limit, `ft` detecting it, a workflow sending recovery input, and the audit row landing.
**Bead coverage:** `ft-d0ez0` (open), `ft-d0ez0.5` (in_progress).
`proof_category: 4 (conformance-artifact) + 5 (quantitative-attestation)`

### G53 — No dogfooding of the observe loop — process (Critical signal)
**Reality:** one `ft.db` on the maintainer's machine, last capture 2026-02-14; stale `watch.lock` since 2026-08-06; the GUI runs daily. The product's author does not run its core loop.
**Bead coverage:** none.
`proof_category: process`

### G54 — Attestation graph has no real release bundle; README points at a nonexistent one — REGRESSED (Critical for the stated moat)
**Reality:** `docs/attestations/0.0.0-dev.json` is `method: unsigned`; no v0.13–v0.15.1 bundle; `ft attestation verify docs/attestations/0.2.0.json` in the 10-minute tour cannot run; DSR quality checks omit `scripts/attestation-verify.sh`; `.github/workflows/release.yml` is the only place the verifier is wired and Rule 0.1 forbids it.
**Bead coverage:** `ft-tf6g3.1` (blocked), `ft-e87u6` (open, stale).
`proof_category: 10 (cryptographic-attestation)`

### G55 — Prompt-active evidence gap makes sends and tx preconditions fail closed for ordinary callers — PARTIAL (Major)
**Reality:** `prompt_active` comes from OSC-133 state reconstructed from stored segments (`main.rs:20067-20232`, `policy.rs:592`). Without a running watcher and shell integration, any untrusted actor (Robot/MCP/Workflow) gets `RequireApproval` (`policy.prompt_unknown`); the CLI tx path never resolves capabilities at all (`ft-zhwa6`). The README tour shows a first `ft robot send` returning `ok: true`.
**Bead coverage:** `ft-zhwa6` (open).
`proof_category: 4 (conformance-artifact) + 2 (property-test)`

### G56 — Kill-switch tiers fail open (security) — UNPROVEN fix (Major)
**Reality:** `ft-l59nq` (P1) has a committed fix (`f8c674376`) and no green proof since 2026-06-15.
`proof_category: 2 (property-test) + 4 (conformance-artifact)`

### G57 — Web `/stream/events` is publisher-less; README says it streams live bus traffic — PARTIAL (Major)
**Reality:** `web.rs:7-13`; `ft-zeo5o` in_progress since June with no update.
`proof_category: 4 (conformance-artifact)`

### G58 — Headline performance claims have no measured release artifacts — PARTIAL (Major)
**Reality:** `docs/perf/headline-claims.json` defines five claims and a `docs/perf/releases/<version>.headline-claims.json` publishing path that has never been populated; capacity envelope blocked; cockpit skipped; rig synthetic.
**Bead coverage:** `ft-7h5da.10.*` (blocked/in_progress), `ft-tf6g3.4` closed.
`proof_category: 5 (quantitative-attestation)`

### G59 — Test-suite health is unknown — UNPROVEN (Major)
**Reality:** `ft-nam3s` baseline 85 failures (2026-07-26); `ft-ziprn` TMPDIR defect open; no August full-suite artifact; DSR quality gate has no retained log on this host.
`proof_category: infrastructure`

### G60 — README embeds two demo GIFs that do not exist — REGRESSED (Minor, credibility)
`proof_category: process`

### G61 — Documentation drift sweep — PARTIAL (Minor)
Schema v43 vs 45; `ft reality-check` CLI verbs vs script; watch startup detector under-claimed; `metrics` feature missing from the matrix; FAQ DB path; "Release CI" wording; counts (1,042 vs 1,043 test files, 1.26 M vs 1.30 M LOC).
`proof_category: process`

### G62 — Upstream WezTerm backport cadence stalled — REGRESSED (Major)
Last `Upstream-WezTerm:` commit 2026-05-13; `PROVENANCE.json` has no backport-batch record.
`proof_category: process`

### G63 — Bead graph no longer a truthful ready queue — REGRESSED (Major)
434 open P0; 193/230 in_progress stale >14 days; monthly create/close: Jun 419/333, Jul 382/117, Aug 414/68; two 2026-07-27 mega-epics hold 663 children with 47 closed.
`proof_category: coordination`

### G64 — Orphaned `mcp_helpers.rs` still on disk — Minor
Requires explicit human deletion authorization (AGENTS.md Rule 1).
`proof_category: process`

### G65 — `main.rs` is a 129,742-line monolith — PARTIAL (Major maintainability)
Contradicts "no tech debt"; slows every review and RCH lane.
`proof_category: substrate`

### G66 — CLI/GUI/mux version-skew has no diagnostic contract — PARTIAL (Major)
The installed reality on the maintainer's Mac is a 0.13.0 app plus a rejected 0.15.1 CLI (`ft-v0.15.1-broken`). `ft doctor` reports the vendored commit mismatch but no "which generation to install" guidance.
`proof_category: 4 (conformance-artifact)`

### G67 — Rule-pack currency has no cadence — UNPROVEN (Major)
Fixtures last changed 2026-06-08; Claude Code / Codex / Gemini output formats change monthly.
`proof_category: 4 (conformance-artifact)`

### G68 — Tantivy secondary index and hybrid ranking production wiring — UNPROVEN (Minor)
`proof_category: 4 (conformance-artifact)`

### G69 — Surfaces this pass could not verify — UNPROVEN (Major, sweep)
Incident-bundle live collectors, notification backends, connector certification pipeline, IPC auth, profile apply spawning, `ft mission run` dispatch, backup/restore e2e (2026-08-31 run produced only `env.json`), session dump/recover.
`proof_category: 4 (conformance-artifact)`

### G70 — Formal-method packaging — Minor
Lean soundness check is not part of any gate; "Stateright" wording describes hand-rolled state-space models; Loom lane cadence undocumented.
`proof_category: 6 (formal-method) + 12 (mechanized-proof)`

---

## 4. Bridge plan

Ordered by vision impact, not ease. Every implementation gap gets a companion test/e2e requirement with retained, structured-log artifacts. "Would beads close it?" is answered per gap.

### G50 → WORKING: a signed, installable, activated release
**Current:** unsigned DSR artifacts; verification-required installer; activation pending by design.
**Target:** `curl | bash` on a clean Apple-Silicon Mac and a clean Ubuntu box ends with `ft --version`, `ft doctor` (exit 0, no error rows), and `frankenterm-mux-server --version` all on PATH, from a generation whose four artifacts carry `.minisig` signatures verified against `release/minisign.pub`.
**Plan:**
1. Wire minisign signing into the DSR release step (`dsr release` post-build hook or `scripts/release/sign-artifacts.sh`), publish `<asset>.minisig` next to every asset and mark `signed: true` in the manifest; `dsr release verify` must fail when any artifact is unsigned.
2. Publish a signed v0.15.2 (or re-sign v0.15.1 as v0.15.1-signed) so the `latest` release is installable; never edit the historical unsigned assets.
3. Implement the minimal activation transaction for the **no-live-mux** case: on `initial` authority with no running mux, the installer may promote the candidate to `current` (create the managed selector and stable entrypoints). Keep the `legacy`/`managed`+live-mux cases pending until the guardian handoff lands, but say so in one line, with the exact command to activate manually.
4. `install.sh --verify` must run the installed `ft doctor` and fail if activation is pending on a first install.
**Success criteria:** e2e script `tests/e2e/test_install_signed_clean_host.sh` (runs in a temp `$HOME`, offline tarball + signature) passes; retained `install-receipt.json` shows `activation: current`; README Quick Install text no longer describes a pending path as the normal case.
**Would beads close it?** Partially — `7xqz4.2` is broad and nothing names the unsigned-release defect. New beads required.
**Complexity:** L (signing S, activation-when-idle M, e2e M). **Serves:** V1, V26.

### G51 → WORKING: `ft` finds and talks to the running FrankenTerm.app
**Current:** core discovery reads config domains and `RUNTIME_DIR/sock`; GUI publishes `gui-sock-<pid>` + `default-<bundle>`; version skew yields opaque errors.
**Target:** with FrankenTerm.app running and no config, `ft robot state` lists its panes within 2 s; with a version-skewed pair, `ft doctor` and every robot envelope carry `robot.mux_version_skew` with both versions and the install command that fixes it.
**Plan:**
1. Teach `frankenterm-core::vendored::discover_canonical_mux_socket` the same order the GUI/client use: explicit config → `WEZTERM_UNIX_SOCKET`/`FRANKENTERM_UNIX_SOCKET` → `client::discovery` default-class symlink (`default-<bundle-id>` on macOS, `wayland-*/x11-*` on Linux) → `RUNTIME_DIR/sock`. Reject dangling symlinks; log the chosen source in `ft doctor --json` (`mux_socket_source`).
2. Add a codec-handshake version check that maps a PDU decode failure or `backend_failure` on `list_panes` into a typed `robot.mux_version_skew` envelope (with `cli_version`, `mux_version`, `bundle_path`).
3. Differential test: same pane list via the GUI socket and via a `frankenterm-mux-server` socket must be identical.
**Success criteria:** native-macOS e2e `tests/e2e/test_ft_attaches_to_running_gui.sh` retained artifact with a non-empty pane list; unit tests for discovery order and skew mapping.
**Would beads close it?** No — no bead exists. **Complexity:** M. **Serves:** V2, V3, V21.

### G52 → WORKING: live-agent proof of the loop (the product's existence proof)
**Current:** fake-adapter closures, unconditional assertions.
**Target:** a retained, redacted artifact bundle proving on the maintainer's Mac: real Claude Code and Codex panes in FrankenTerm.app → `ft watch --auto-handle` → a real usage-limit/compaction detection → workflow send → audit row → `ft robot events` shows it, at 3 panes; then 20 panes with FTS search and a policy denial; 50 panes with fleet-pressure tier transitions asserted, not logged.
**Plan:** reopen `ft-d0ez0.1-.3` semantics under new children that (a) require `mux_backend == vendored` and `adapter == live`, (b) fail when any assertion is informational, (c) bind the artifact to CLI/GUI versions and the bundle SHA, (d) publish under `docs/attestations/proofs/live-loop-<tier>.json`. Provide `scripts/live-loop-proof.sh` that stages the panes via `ft robot profile apply` and records the run with the recorder so it can be replayed.
**Success criteria:** three signed artifacts with `status: pass` and `adapter: live`; `ft-d0ez0.5` closed with the false-green assertions replaced.
**Would beads close it?** Partially (`ft-d0ez0` exists but its closed children are the problem). **Complexity:** XL. **Serves:** V3, V5, V14, V21.

### G53 → WORKING: dogfooding as a standing gate
**Plan:** run `ft watch` continuously on the maintainer's workspace(s); add `scripts/dogfood-status.sh` that reports last capture age, event count, and workflow runs from every `ft.db`; make "last capture < 24 h on the maintainer host" a release-checklist item recorded in the attestation bundle.
**Success criteria:** `docs/attestations/doctrine/dogfood-status.json` populated per release. **Complexity:** S. **Serves:** V3, V27.

### G54 → WORKING: first real signed release attestation bundle
**Plan:** unblock `ft-tf6g3.1` by (1) allowing `deferred_slots` for the target-class and capacity slots with explicit reasons, (2) adding `scripts/attestation-verify.sh --strict-deferred` and `ft attestation verify` to the DSR quality checks in `~/.config/dsr/repos.yaml`, (3) signing with minisign now (cosign later), (4) fixing the README to cite `docs/attestations/<latest>.json`, (5) removing or archiving the 25 GitHub workflow files under Rule 0.1 with a one-line pointer to DSR (deletion needs owner authorization).
**Success criteria:** `docs/attestations/0.15.2.json` verifies offline with a real signature; README tour command runs. **Would beads close it?** Partially. **Complexity:** M. **Serves:** V17, V25.

### G55 → WORKING: prompt-active evidence is resolvable everywhere, and the tour is honest
**Plan:** (1) close `ft-zhwa6` by pre-resolving capabilities on all five CLI sites; (2) when no OSC-133 evidence exists, make the envelope say so (`hint: enable shell integration: ft setup shell-integration`) and ship `ft setup shell-integration` that installs the bundled OSC-133 scripts; (3) `ft doctor` row "prompt-state evidence" per pane; (4) rewrite the README tour so the first send shows the RequireApproval path unless integration is present.
**Success criteria:** proptest over capability resolution; e2e where a pane with integration sends `ok:true` and one without gets `policy.prompt_unknown`. **Complexity:** M. **Serves:** V6, V12.

### G56 → WORKING: kill-switch tiers proven
**Plan:** run the `ft-l59nq` tests on RCH; add a conformance matrix (tier × action kind) as `docs/attestations/proofs/killswitch-tier-enforcement.json`. **Complexity:** S. **Serves:** V6.

### G57 → WORKING: `/stream/events` carries live events
**Plan:** storage-tail → `EventBus` bridge for standalone `ft web` (poll `events` table by id cursor at `max_hz`), plus an in-process mode when `ft watch --web` runs; e2e proves a detection appears on the SSE stream within 1 s. **Complexity:** M. **Serves:** V9.

### G58 → WORKING: measured headline claims per release
**Plan:** make `docs/perf/releases/<version>.headline-claims.json` a DSR quality output: run the five benches on the declared local M-series baseline, emit distributions, apply the `ebci-upper-bound` gate; replace the synthetic rig metrics with real capture-pipeline measurements (close `ft-7h5da.10.3.2`, `.10.4.3`). **Complexity:** L. **Serves:** V18, V14.

### G59 → WORKING: known test-suite state per release
**Plan:** fix `ft-ziprn` (TMPDIR) at the source; publish `docs/attestations/doctrine/test-suite-status.json` (pass/fail/ignored counts, worker, SHA) from the DSR quality run; burn the 85-failure baseline to zero or explicit `#[ignore]` with bead links. **Complexity:** M. **Serves:** V20.

### G60 → WORKING: demo recordings exist or README stops embedding them
**Plan:** render `scripts/demo.tape` with vhs now; either stage the swarm for `demo-full.tape` or remove the second embed until `ft-xl2kc.1` un-defers. **Complexity:** S.

### G61 → WORKING: documentation truth sweep
**Plan:** one bead with the enumerated corrections; add `metrics` to the feature matrix; make `scripts/stamp-readme-counts.sh` also stamp `SCHEMA_VERSION`. **Complexity:** S.

### G62 → WORKING: upstream backport cadence restored
**Plan:** run the AGENTS.md weekly process for 2026-05-13 → now; record batches in `PROVENANCE.json` (`backport_batches[]`); add `scripts/check-upstream-backport-due.sh` (advisory, 14-day threshold). **Complexity:** M (first batch L).

### G63 → WORKING: truthful ready queue
**Plan:** (1) priority re-normalization: cap P0 to ≤ 25 beads that gate the next release; demote the rest of `7xqz4`/`4tenz` children to P2/P3 with a one-line rationale; (2) release 193 stale in_progress claims per Rule SO-8 (broadcast, then `--status open --assignee ''`); (3) require every new epic to declare a "first vertical slice" child that produces a user-visible result; (4) weekly `scripts/ft-reality-check.sh silent-close-audit`. **Complexity:** M. **Serves:** V27.

### G64 → WORKING: orphan removed with authorization
**Plan:** bead asks the owner for written permission to delete `crates/frankenterm-core/src/mcp_helpers.rs`; add a guard test that fails if a `.rs` file under `src/` is not reachable from `lib.rs`. **Complexity:** S.

### G65 → WORKING: `main.rs` split isomorphically
**Plan:** apply the de-monolithize discipline: measure seams (command families), split into `crates/frankenterm/src/commands/<family>.rs` with zero behavior drift proven by the golden robot envelope matrix; sequence robot → watch → mission/tx → session → attestation. **Complexity:** XL (staged). **Serves:** V30.

### G66 → WORKING: version-skew contract
Folded into G51 step 2 plus an installer rule: never leave a CLI generation whose codec version differs from the installed app without printing the pairing command. **Complexity:** S.

### G67 → WORKING: rule-pack currency cadence
**Plan:** monthly bead template that captures fresh Claude Code / Codex / Gemini output for the 12 highest-value rules, runs `ft robot rules lint --fixtures --strict`, and records fixture provenance (CLI version, date) in the corpus; add `scripts/check-rule-corpus-age.sh`. **Complexity:** S recurring.

### G68/G69 → WORKING: verification sweep of unverified surfaces
**Plan:** one sweep bead per surface family with a named check, an artifact path, and a status of pass/blocked/skipped; nothing in the README stays "Supported" without a linked artifact. **Complexity:** M.

### G70 → WORKING: formal-method packaging
**Plan:** add the Lean check and the Loom lane to DSR quality (or a scheduled RCH lane) with retained logs; reword README "Stateright" rows to "explicit-state model (hand-rolled)"; publish `docs/attestations/proofs/formal-lanes.json`. **Complexity:** S.

### Dependency sketch
G50 ← (signing S) ; G51 independent ; G52 ← G51, G55 ; G53 ← G51 ; G54 ← G50 signing, G59 ; G58 ← G52 (real rig) ; G63 independent and first ; G65 last.

---

## 5. Would completing all open beads close the vision gap?

No. The open graph is dominated by two July programs whose closure would qualify the product at scale, but the **entry ramp** (install, attach, first detection, first signed bundle) has regressed and is only partially tracked. Eleven gaps above have no bead (G50, G51, G53, G60, G61, G62, G63, G64, G65, G67, G70) and six more sit on stale in_progress/blocked beads that need an unblocking plan (G54, G56, G57, G58, G59, G55). The correct sequencing is: entry ramp first (G50, G51, G52, G53, G54), then truthfulness (G55–G61), then cadence and structure (G62–G70), while the scale programs continue at demoted priority.

---

## 6. Program-health findings (beads)

| Signal | Value |
|---|---|
| Open + in_progress + blocked + deferred | 940 |
| Open P0 | 434 (252 in `7xqz4`, 167 in `4tenz`) |
| in_progress with no update > 14 days | 193 of 230 |
| Created / closed per month | Jun 419/333 · Jul 382/117 · Aug 414/68 |
| Commits per month | Jun 1,016 · Jul 240 · Aug 1,742 |
| `ft-tf6g3` children | 75 closed / 5 blocked |
| `ft-7h5da` W0–W12 | 60 closed / 11 in_progress / 37 open / 36 blocked |
| `7xqz4` (2026-07-27) | 4 closed / 44 in_progress / 237 open / 1 blocked |
| `4tenz` | 43 closed / 77 in_progress / 257 open |
| Cycles (active scope) | 0 |

August's 1,742 commits went mostly to mux/codec/pty-guardian/session/checkpoint work (feat 515, fix 465, test 241) — real engineering on continuity and reconnect — while the install/attach ramp was not exercised.

---

## 7. Bead epic and children (created 2026-09-01 with `br` only)

**Epic:** `ft-xxfwy` — `[reality-check-2026-09-01] Entry-ramp truth epic`. Labels: `reality-check`, `reality-check-2026-09-01`. Child range: `ft-xxfwy.1` … `ft-xxfwy.30`. Every child carries a `proof_category:` line (taxonomy IDs 1–12 or a non-proof slug) and a `Source:` line pointing at this plan.

| Bead | Gap | Type / P | Title (short) | Depends on |
|---|---|---|---|---|
| ft-xxfwy.1 | G50 | task P0 | Sign DSR release artifacts with minisign; manifest `signed:true`; verify fails on unsigned | — |
| ft-xxfwy.2 | G50 | task P0 | Publish signed v0.15.2 + clean-host installer e2e | .1 |
| ft-xxfwy.3 | G50 | feature P0 | Installer activation on idle hosts; `--activate`; live-host next_action | — |
| ft-xxfwy.4 | G50 | test P0 | Installer activation e2e (idle, live hold, failpoint rollback) | .3 |
| ft-xxfwy.5 | G51 | feature P0 | Ranked mux socket discovery incl. GUI default-class symlink | — |
| ft-xxfwy.6 | G51 | test P0 | Discovery unit/proptest/differential/native tests | .5 |
| ft-xxfwy.7 | G51/G66 | feature P1 | Typed `robot.mux_version_skew` + doctor pairing + installer guidance | .5 |
| ft-xxfwy.8 | G51 | test P0 | Native macOS e2e: `ft` attaches to running FrankenTerm.app | .5, .7 |
| ft-xxfwy.9 | G52 | task P0 | Live-loop proof tier 1 (3 real agent panes) | .8, .12 |
| ft-xxfwy.10 | G52 | task P1 | Live-loop tiers 2/3 with authoritative gates; retire false-green assertions | .9 |
| ft-xxfwy.11 | G53 | task P1 | Dogfood status gate + launchd template + attestation slot | .8 |
| ft-xxfwy.12 | G55 | feature P1 | Prompt-active evidence everywhere; close ft-zhwa6; `ft setup shell-integration` | — |
| ft-xxfwy.13 | G55 | test P1 | Prompt-capability proptest + two-pane e2e + tx precondition e2e | .12 |
| ft-xxfwy.14 | G56 | task P1 | Kill-switch tier enforcement proof (ft-l59nq) + conformance artifact | — |
| ft-xxfwy.15 | G54 | task P0 | First real signed attestation bundle + DSR verifier wiring + README path fix | .1, .17 |
| ft-xxfwy.16 | G54 | task P2 | Retire `.github/workflows` under Rule 0.1 (owner authorization) | .15 |
| ft-xxfwy.17 | G59 | task P1 | Test-suite status artifact; fix ft-ziprn; burn down ft-nam3s baseline | — |
| ft-xxfwy.18 | G57 | feature P2 | `/stream/events` live: storage-tail bridge + `ft watch --web` | — |
| ft-xxfwy.19 | G57 | test P2 | SSE live-events e2e | .18 |
| ft-xxfwy.20 | G58 | task P1 | Measured headline claims per release; real-pipeline load rig | .9 |
| ft-xxfwy.21 | G60 | task P2 | Demo recordings + README asset check | .8 |
| ft-xxfwy.22 | G61 | docs P2 | Documentation truth sweep | — |
| ft-xxfwy.23 | G62 | task P1 | Restore weekly upstream backport cadence + provenance batches | — |
| ft-xxfwy.24 | G63 | task P1 | Program-health reset (P0 ≤ 25, stale claims, vertical-slice rule) | — |
| ft-xxfwy.25 | G64 | task P3 | Remove orphaned `mcp_helpers.rs` (authorization) + guard test | — |
| ft-xxfwy.26 | G65 | task P2 | Split `main.rs` isomorphically | — |
| ft-xxfwy.27 | G67 | task P2 | Rule-pack currency cadence + fixture provenance | — |
| ft-xxfwy.28 | G68/69 | task P2 | Verification sweep of unverified Supported surfaces | — |
| ft-xxfwy.29 | G70 | task P3 | Formal-method lanes in DSR quality | — |
| ft-xxfwy.30 | final | test P0 | README Quick Install + 10-Minute Tour verbatim on clean hosts (acceptance gate) | .2, .3, .5, .12, .15, .22 |

Existing beads this epic unblocks or subsumes (not re-parented): `ft-zhwa6` (via .12), `ft-l59nq` (via .14), `ft-zeo5o` (via .18), `ft-nam3s` + `ft-ziprn` (via .17), `ft-tf6g3.1` + `ft-e87u6` (via .15), `ft-d0ez0.5` (via .10), `ft-7h5da.10.3.2` + `ft-7h5da.10.4.3` (via .20), `ft-xl2kc.1` (via .21).

**Ready-to-work entry points (no blockers):** .1 signing, .3 activation, .5 discovery, .12 prompt evidence, .14 kill switch, .17 suite status, .18 SSE, .22 docs, .23 backports, .24 program health, .25–.29.

**Recommended order:** .24 (make the queue truthful) in parallel with .1 + .3 + .5; then .2/.4/.6/.7/.8; then .12/.13 → .9 → .10/.20; .17 → .15 → .16; .22, .21, .11 alongside; .30 last.

---

## 8. Ambition and refinement record

- **Phase 1 (assessment):** evidence gathered from AGENTS.md/README (fully read), the two predecessor plans, `br`/`bv`/JSONL, live probes of the installed 0.13.0 and 0.15.1 binaries against the running 0.13.0 GUI, the v0.15.1 release manifest, the DSR repo configuration, install.sh activation and signature paths, and the code paths in `runtime.rs`, `workflows/runner.rs`, `tx_execution.rs`, `policy.rs`, `vendored.rs`, `web.rs`, `main.rs`.
- **Phase 2 (bridge plan):** §4.
- **Phase 3a (beads):** 30 beads, 23 blocking edges, 0 cycles.
- **Phase 4 (ambition):** the entry-ramp framing replaced a first draft that listed gaps subsystem by subsystem; the acceptance gate (.30) and the program-health reset (.24) were added so closure means a stranger can run the README, not merely that beads closed. Round-2 elevation: every proof bead binds its artifact to CLI/GUI generation and host, and skipped runs must record reasons (no silent pass).
- **Phase 5 (refinement passes):** pass 1 added test companions (.4, .6, .13, .19) and the final integration bead; pass 2 added operator surfaces (`ft doctor` rows for socket source, pairing, prompt evidence, dogfood, web event source) and degradation behavior to each implementation bead; pass 3 added the authorization requirements (Rule 1) to the two removal beads (.16, .25) so no agent deletes files on its own; pass 4 re-checked every existing bead this epic touches and recorded the subsumption list above instead of re-parenting.
- Graph audit: `br dep cycles --json` → 0 active cycles after wiring.

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
- Remote test lane launched during this run: the `frankenterm-core` library suite with three jobs on RCH worker hz2 (`CARGO_TARGET_DIR=/tmp/ft-rc20260901-core-lib`, `TMPDIR=/tmp`). Result: see the line below.

**Test-lane result (remote, hz2, 2026-09-01T19:0x–19:3x Z):** `frankenterm-core --lib` → 31,256 passed / 4 failed / 2 ignored, 130.68 s. Failures: `api_schema::tests::current_version_parses`, `snapshot_engine::tests::intelligent_exact_threshold_boundary`, `snapshot_engine::tests::periodic_mode_ignores_triggers`, `snapshot_engine::tests::run_periodic_with_cx_mid_flight_cancel_exits_quickly`. This is the first retained post-July full core-lib result; the July baseline (85 failures) is stale and the remaining four are recorded on `ft-xxfwy.17`. Log retained in the session scratchpad (`core-lib-test.log`); a copy is attached to the bead comment.

## 11. Execution log — same day (2026-09-01/02)

Work started on the entry ramp immediately after the plan was published. Everything below is committed on `main`; "proof" means a retained remote RCH lane transcript, "native" means a dev build run on the maintainer's Mac (a signal, not proof).

| Gap | Landed (commit) | Proof state |
|---|---|---|
| G51 socket discovery | `config::gui_socket` shared naming; ranked `discover_mux_socket_ranked` + `MuxSocketSource`; client/GUI re-exports; `build_unified_client` dials the discovered path; doctor `mux socket` row (f862716ce, 002df4c37, 3ae637088, b02802df5, 82b91e9b4, cbb0a396f) | client discovery 10 pass, core `wezterm::tests` 197 pass, config `gui_socket` 4 pass (hz2); native doctor shows `source: gui_published` against the running app |
| G51/G66 version skew | `WeztermError::VersionSkew`, `robot.mux_version_skew`, FT-1025, doctor pairing recommendation (0f64ca942, b02802df5) | consolidated lane pending (lanes-6) |
| G55 prompt evidence (CLI half) | five CLI tx sites resolve pane capabilities (`resolve_tx_contract_capabilities`) | `cargo check -p frankenterm --bin ft` pass (hz2) |
| G57 web SSE | storage → bus tail (`StorageEventTail`) in `ft web`; default on (a950faf99, 744e52729) | `web::` lane pending (lanes-6) |
| G50 signing/activation | `scripts/release/verify-release.sh` (exits 1 on v0.15.1); `install.sh --activate <gen> --idle-host-confirmed` (da3b0e1fd) | verifier exercised against the real release; activation refusal paths smoke-tested; end-to-end activation NOT tested (ft-xxfwy.4) |
| G53 dogfood | `scripts/dogfood-status.sh` (83e55a72c) | run: stale, newest capture 4793 h |
| G54 gates | 25 workflow files retired; `scripts/release-gates.sh` (static + `--cargo`); tests/artifacts repointed; attestation gate builds then verifies the dev bundle | 28 of 28 static gates pass after fixes (bead-structure, runtime-proof census, coupling baseline) — final rerun pending |
| G56 doctor honesty | policy rows labelled process-local (784bb8870); persistence gap recorded on ft-xxfwy.14 | kill-switch tests on lanes-6 |
| G61 docs | README/AGENTS truth sweep (schema v45, `<version>.json`, DSR wording, detector, metrics row, Stateright wording, FAQ path, demo embeds removed) | `stamp-readme-counts.sh --check` pass |
| G63 program health | 606 P0/P1 → P2; 71 stale claims released; 26 broadcast; open P0 = 25 | — |
| G64 orphan | `mcp_helpers.rs` removed (authorized); `no_orphan_source_files` guard test; census re-blessed | guard test lane pending (lanes-6) |
| G68/G69 sweep | finding: no production path created agent profiles → `ft robot profile create` added (5945f1127) | handler tests on lanes-7 |
| test baseline | fresh core-lib run 31,256 pass / 4 fail → 1 fixed (f494eb398), 3 snapshot timing fixes | lanes-6 |

**Headless attach smoke (2026-09-02, dev build, same generation).** A dev `frankenterm-mux-server` plus the dev `ft` from the same build were run on the maintainer's Mac to get a same-codec attach without the 0.13.0 app. Result: the mux server aborted (SIGABRT) on the first client connect. Root cause: `LocalListener::run` (shared by the headless server and the GUI's published-socket listener) called `reservation.spawn_local` from the listener thread; async_task's thread check panicked on the main thread's first poll and again during drop, so the whole server died and the client only saw "codec_version_handshake: response read EOF". The promise crate documents this exact hazard and ships `handoff_to_main_thread_local`; the listener now uses it (`admit_connection`) with a regression test that aborted under the old code. This is the concrete mechanism behind G51 for same-generation pairs on HEAD; it is invisible against the 0.13.0 app because that build pre-dates the admission refactor. Proof lane pending at the time of writing; the smoke rerun is recorded below once it passes.

With the fix in place the same smoke passed end to end (dev build, 2026-09-02 01:47 UTC): `ft doctor` reports the mux socket (source: environment) and "1 pane(s) detected via vendored client"; `ft list` returns pane 0; `ft watch --foreground` opens a vendored pane streaming subscription; `ft send --no-paste` sets the pane title to `codex` and echoes the Codex usage-limit message; `ft events` returns event 1, rule `codex.usage.reached`, severity critical, `reset_time = "3:00 PM"`. That is tier 1 of the live loop (§5) on a real mux, minus auto-handle and minus a release artifact. The recipe is now `scripts/smoke/headless-mux-observe.sh`. Two things it also surfaced: sends default to bracketed paste, which zsh does not execute, and `codex.usage.reached` needs both its anchor and a case-sensitive `try again at` regex (a capitalized `Try again at` line produced no event; the real Codex casing was not verified).

Two follow-on defects were fixed from the same smoke: the real Codex binary prints " Try again at " with a capital T, so the case-sensitive `codex.usage.reached` regex missed the primary usage-limit message (now `(?i)`, tested on the real string, e903495ee); and a pane spawning during the handshake makes the mux broadcast a unilateral `TabResized` before the registration reply, which the client's phase gate treated as an authority violation and poisoned the connection (unilateral PDUs now pass the phase gate, scripted-server test, 0f00b7cf5).

Two smaller findings from the same run: `frankenterm-mux-server --config-file` is silently ignored (the server installs no logger, so a config load error prints nothing and it falls back to `RUNTIME_DIR/sock`), and the orphan guard found four more tracked orphan files in frankenterm-core (ft-xxfwy.31; baselined, deletion needs owner authorization).

Not done today: signed release (needs an operator-driven `dsr build/release` with signing on), same-generation app for the native attach e2e, live-loop tier 1, `ft setup shell-integration`, kill-switch persistence.

## 10. Successor note

The next full reality-check must cross-link this plan, `docs/reality-check-bridge-plan-2026-05-12.md`, and `docs/reality-check-bridge-plan.md`, and must start from `ft-xxfwy`'s terminal state. It becomes due per `docs/process/reality-check-discipline.md` (90 days, minor-version change, ≥ 50 open beads, contract churn, or headline-claim growth). Do not overwrite this file; revise it in place only within this run.
