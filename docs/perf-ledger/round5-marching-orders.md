# Round-5 Alien Optimization Gauntlet — Swarm Marching Orders

Epic: **ft-round5-gauntlet-lw0s7**. Discipline source: `docs/perf-ledger/round4-keep-ledger.md` +
`round4-negative-results.md` (10 keep-gate rules, 8 retry forms) + AGENTS.md. Goal: quantify the 19
round-4 default-OFF optimizations on a quiet host and ship v0.8.0, plus bold new ideas.

## NON-NEGOTIABLE RULES (every pane)

1. **Claim your bead first:** `br update --status in_progress <bead> --assignee <your-pane-name>`.
2. **File ownership is exclusive.** Edit ONLY the files listed in your section. If you must touch a
   shared core file, coordinate via Agent Mail / a bead comment first. Keep the tree COMPILING at all
   times — a mid-edit non-compiling `frankenterm-core` file fails EVERY sibling's RCH proof (ft-ch3nm).
3. **Commit code-first, FAST.** Small coherent commits. `git add <your files> && git commit` as a
   single invocation (sibling agents sweep staged files otherwise). Run `ubs <changed-files>` first.
4. **Proofs are RCH-remote, fail-closed.** Never count `[RCH] local`. Template:
   ```
   RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 CARGO_NET_GIT_FETCH_WITH_CLI=true \
     rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-<bead>-<purpose> \
     cargo <check|test|bench --no-run> -p <pkg> <filters>
   ```
   ovh-a/ovh-b are drained (canonical-mkdir fails) — vmi*/hz1/hz2 work. `rch cache clean` on ENOSPC.
   Use TARGETED filters; never the whole `core --lib` (it times out as one job).
5. **Every optimization stays DEFAULT-OFF** behind its existing feature/env/config gate. Bench work
   does NOT flip defaults — the orchestrator promotes winners after the local A/B.
6. **Correctness proof before claiming done:** the relevant golden/property/oracle test stays
   byte-identical (or your new bench compiles `--no-run` green via RCH).
7. **Report back** in your pane when done: `DONE <bead>: <commit-sha> — <one-line proof result>`.
   If blocked >30min on infra (RCH wedge), commit code-first + say `BLOCKED <bead>: <reason>` and stop
   hammering; the orchestrator reroutes.

## A/B bench contract (for bench-authoring/wiring beads)

The orchestrator runs `scripts/round4-bench-ab.sh --local` per flag on the quiet Mac. Your bench must
let the driver express the two arms via ONE of:
- `--gate feature:<NAME>` — bench's benched code path differs with `--features <NAME>` (baseline=off).
- `--gate env:VAR=ON[/OFF]` — bench reads an env var at run time (build once, run twice).
So: the benched function must actually honor the gate, and BOTH arms must be reachable from the public
API the bench calls. If a config flag isn't reachable from the bench, expose a minimal
`#[doc(hidden)] pub` setter/introspector rather than adding a public config field (round4 isolation
learning) — do NOT add a field to a struct with sibling-owned full-literal constructors.
Pick workloads where the optimization SHOULD win (named per bead) so the A/B is a fair test.

---

## PANE ASSIGNMENTS

### cod_1 — bead ft-round5-gauntlet-lw0s7.4 — patterns benches (A1)
Own: `crates/frankenterm-core/benches/` (patterns bench files — use NEW files, don't collide with cod_2/cod_3) + the `[[bench]]` entries you add in `crates/frankenterm-core/Cargo.toml`.
Author 3 isolating Criterion benches, each runnable as a feature A/B:
- **Q5 Teddy** (`feature teddy-prefilter`): low-match-rate chunk workload (most chunks match NO rule) over the public detection API; the win is prefilter rejecting chunks before fancy_regex.
- **Q6 fingerprint dedup** (`feature patterns-fingerprint-dedup`): high seen-key churn dedup workload (many distinct repeated lines) exercising the dedup cache insert/evict path.
- **M5 MPHF dispatch** (`feature patterns-mphf-dispatch`): high-Aho-Corasick-hit ("chatty" agent output) workload exercising anchor→rule-bitset routing.
Verify each feature actually changes the benched path. Proof: RCH `cargo bench --no-run -p frankenterm-core --features <each>` green + existing detection-stream golden unchanged.

### cod_2 — bead ft-round5-gauntlet-lw0s7.5 — scroll/mem benches (A1)
Own: `crates/frankenterm-core/benches/` (scrollback/memory/cache bench files — NEW files).
- **Q1 prefix-sum** (env `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` — VERIFY the real gate at `scrollback_tiers.rs:177`): deep-scroll workload with hundreds of warm pages exercising `locate_offset`/`tier_for_offset`.
- **M9 PID mem** (config `memory.dampening=pid`, `fleet_memory_controller.rs`): synthetic memory-pressure replay measuring eviction magnitude / tier-flap oscillation vs hysteresis baseline.
- **S3-FIFO** (config `cache.eviction=s3fifo`, lfucache): scan-heavy one-hit-wonder access trace measuring hit-rate at equal capacity vs LFU.
M9/S3-FIFO wins are hit-rate / evicted-bytes (not ns) — emit the metric clearly so the orchestrator can adjudicate even if the timing A/B is flat. Proof: RCH `cargo bench --no-run` green.

### cod_3 — bead ft-round5-gauntlet-lw0s7.6 — storage/tailer/simd bench wiring (A2)
Own: `crates/frankenterm-core/benches/` WAL + tailer + simd_scan bench files (wire toggles into existing benches; NEW files only if cleaner).
- **Q2 + M8 WAL group-commit** (config `storage.group_commit_events`/`writer_blocking_recv` for Q2; `storage.group_commit=adaptive` for M8): ~200-pane sustained-write burst workload; arms differ by config.
- **M7 tailer cadence** (config `ingest.cadence_model=predictive` vs backoff): capture-cadence workload.
- **M1 simd_scan DFA** (`feature ansi-dfa-table`): ANSI-dense workload (`simd_scan_ansi_heavy`), arm with feature vs without.
Bench-file edits preferred (low contamination). If a config flag isn't reachable from the bench, add a `#[doc(hidden)] pub` setter — not a public config field. Proof: RCH `cargo bench --no-run` for each arm.

### cod_4 — bead ft-round5-gauntlet-lw0s7.7 — M3 SoA bench + C1 GUI hang fix
Own: `crates/frankenterm-gui/` (benches + `src/main.rs`) + `frankenterm/client/src/client.rs` (connect path).
- **M3 SoA glyph** (env `FT_MOONSHOT_INSTANCED_GLYPH_QUADS`): turn the `ft_3r0yk/soa_quad_staging_toggle` stub into a real frame-time A/B under glyph-dense frames (extend `input_to_photon`).
- **C1 hang harden:** `main.rs:778` `block_on(Publish::try_spawn())` → `Client::new_unix_domain` → `unix_connect_with_retry` → `UnixStream::connect()` (`client.rs:566`) is a BLOCKING syscall with NO timeout; a stale socket hangs startup. Add a ~500ms connect timeout that fails fast and falls through to window creation. Mirror the `promise::spawn::sleep`+`select!` timeout pattern at `main.rs:1533`. Diagnose root cause first (AGENTS 1.5). Verify: LLDB launch with `FRANKENTERM_LUA_CONFIG=1` reaches "Renderer initialized" with no hang.

### cod_5 — bead ft-round5-gauntlet-lw0s7.8 — distributed-mTLS test split (B1)
Own: `crates/frankenterm-core/src/distributed.rs` + NEW `crates/frankenterm-core/tests/integration_distributed_tls.rs` + `crates/frankenterm-core/Cargo.toml` (`[[test]]`).
Move the ~36 real-TLS tests (`tls_handshake_*`, `mtls_handshake_*`, `tls_rejects_*` — they bind real TCP, gen certs, use 2–5s timeouts) out of the `#[cfg(test)]` module in distributed.rs into a `[[test]]` integration target with `required-features = ["distributed"]`, so default `--lib` is fast. CORE editor — commit fast, keep tree compiling. Proof: RCH `cargo test -p frankenterm-core --lib` (fast, green) + `cargo test -p frankenterm-core --test integration_distributed_tls --features distributed` (green).

### cc_1 — bead ft-round5-gauntlet-lw0s7.9 — bocpd stabilize + SR bench toggle (B2)
Own: `crates/frankenterm-core/src/bocpd.rs` + bocpd bench file.
- **B2 stabilize** `shiryaev_roberts_detects_runaway_sooner_at_matched_false_alarm_rate` (`bocpd.rs:1284`): the strict `sr_delay < bocpd_delay` inequality flakes under FP-order load. Diagnose the FP non-determinism (mul_add/ln_gamma ordering), then add a justified tolerance band (accept ties: `sr_delay <= bocpd_delay`, plus the existing zero-false-alarm assert) and document WHY ties are valid at matched false-alarm rate. Keep the corpus deterministic.
- **SR bench toggle** (config `bocpd.detector=shiryaev_roberts` vs `bocpd`): wire the existing bocpd benches to A/B the two detectors on a synthetic-changepoint workload.
CORE editor — commit fast. Proof: RCH `cargo test -p frankenterm-core --lib bocpd` green (repeat 2x for flake) + `cargo bench --no-run`.

### cc_2 — bead ft-round5-gauntlet-lw0s7.10 — parser printable-run batching (D1)
Own: `frankenterm/escape-parser/` (parser print path) + `frankenterm/term/src/terminalstate/performer.rs`.
Per ft-1dlpt candidate-1: in vtparse GROUND state, scan a maximal printable UTF-8 run (stop at any C0/C1, ESC, DEL, invalid/incomplete UTF-8) and emit ONE `Action::PrintString` for the run; fall back to scalar byte-stepping for non-ground/split/invalid. Behind a default-OFF cfg/feature with an equivalence gate comparing per-byte actions vs batched across chunk boundaries. Vendored editor — commit fast. Proof: escape-parser conformance corpus `parse_as_vec` equivalence (byte-identical action stream) + term golden, via RCH `cargo test -p escape-parser` / `-p term`.

### cc_3 — bead ft-round5-gauntlet-lw0s7.11 — concurrent-search-while-streaming bench (E1)
Own: NEW bench file under `crates/frankenterm-core/benches/` (and/or a small harness).
Author a bench that, at HIGH pane count (e.g. 100–200 panes), runs concurrent search reads while capture appends stream into per-pane scrollback (`Arc<Mutex<Terminal>>`), instrumented so we can see whether scrollback read/render lock-wait is ABOVE NOISE. This is the M6 retry-predicate evidence harness (none exists today). Bench-only file (low contamination). Proof: RCH `cargo bench --no-run` green; the orchestrator runs it locally for the M6 decision (E2).

---

## Queued / orchestrator-owned (do NOT start unless reassigned)
- D2 (.12 CSI/OSC dispatch) — queued for cc_2 after D1.
- D3 (.13 fresh mining), A0 (.1), A3 (.2), A4 (.3), B3 (.14), B4 (.15), E2 (.16), F (.17) — orchestrator.
