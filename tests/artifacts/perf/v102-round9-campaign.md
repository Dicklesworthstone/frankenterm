# v0.10.2 — Round-9 Alien Optimization Gauntlet Campaign Record — FINAL

> **Round-9 = TARGETED-FINISH** (operator-chosen). Resumed post-v0.10.1 (`7fb968b17`). Round-8 declared the
> campaign "substantially converged" with one live carryover (`ft-ui1xn`) + one parked lever (`ft-yjihu.1`).
> Round-9 closed both — and both wins are **removals/parked-levers**, exactly the round-8 nuance. The
> optimization campaign is now declared **FULLY converged**. Ledgers:
> `docs/perf-ledger/round9-{keep-ledger,negative-results}.md`.

## Charter (operator-locked)

- **Scope:** targeted-finish — resolve ft-ui1xn (the carryover), promote ft-yjihu.1 (the parked WAL lever),
  harvest the already-landed hot-path beads, delete dead scan_pipeline, declare FULL convergence. No
  speculative new mining (operator recommended against (c)).
- **Cadence:** autonomous to convergence.
- **End state:** cut **v0.10.2** (prepare on main + coordinate the multi-host build).
- **Bench host:** local Mac + deterministic harnesses; correctness proofs RCH-remote / fail-closed.

## The distilled 5-round verdict (drove the round)

After v0.7→v0.10.1 the per-op CPU micro-space is **exhausted**. The only pattern that ever delivered: replace
per-element work with a bulk/single-pass op on a genuinely-live hot frame, OR collapse a complexity class —
and now its inverse: **REMOVE an existing prefilter whose overhead exceeds its savings**. Round-9 found the
single largest remaining detection-path win was exactly that removal (ft-ui1xn). Hence: targeted-finish, not
mining.

## Workstreams

| Item | Class | Status |
|---|---|---|
| ft-zhj63 B0-correction (the dead `scan_pipeline.process` #1 frame is a phantom) | profile | **DONE** — `detect_with_context` is the corrected #1 at 63.8% (`9137b11ab`, `round9_profile_realistic_workloads.rs`) |
| ft-ui1xn — remove the net-negative `quick_reject` Bloom prefilter | new lever (removal) | **PROMOTED default-off** (`9137b11ab`); +35–43% Mac cv≤5%, 22.76% of fleet self-time, byte-equiv RCH `hz2` |
| ft-yjihu.1 — WAL skip-startup-checkpoint | parked lever | **PROMOTED default-on** (`dd0043b79`); +74% dirty-WAL startup, no regression, RCH `hz2` 8+1 passed |
| Harvest: ft-gkh4p, ft-4lq0i | already-landed | **CLOSED** green (97-passed core lib run) |
| Harvest: ft-uyt88 | already-landed | **FALSE-OPEN caught** — regression test hangs on this macOS host; revert experiment proved the BufReader change is NOT the cause; kept OPEN for Linux/CI |
| scan_pipeline deletion | hygiene | **DONE** (`6f8089935`) — 4260 dead lines removed; round-6/7 denominators rewired to `detect_with_context`; pattern_trigger kept |
| Cut v0.10.2 | release | **PREPARED on main** — version bump + ledgers + changelog + darwin-arm64 local; multi-host build coordinated with operator |

## Scorecard

### Promoted → default flip
| Idea | Win | Proof |
|---|---|---|
| ft-ui1xn remove quick_reject Bloom prefilter | **+35–43% per-delta detection** (Mac cv≤5%); **22.76% of total fleet detection self-time** eliminated; byte-equivalent | local Mac release-perf A/B (cv 2.0%) + RCH-remote `hz2 (1183.9s)` byte-equivalence across 256 deltas (6 match-present) |
| ft-yjihu.1 promote WAL skip-checkpoint default-on | **+74% dirty-WAL startup** (724µs vs 2789µs, ~2.07ms saved), no regression on clean/large branches | local Mac startup A/B (cv≤10%) + RCH-remote `hz2 (1143.4s)` `round8_wal_recovery` 8/8 + `round9_wal_startup_time` |

### Hygiene
| Idea | Status | Proof |
|---|---|---|
| Delete dead scan_pipeline (4260 lines) | **DONE** (`6f8089935`) | `cargo check --all-targets` green; harnesses pass; RCH-remote cascade compile |

### Negative evidence = a win
| Idea | Status | Retry form |
|---|---|---|
| ft-uyt88 mux BufReader | **FALSE-OPEN caught, kept open** — test hangs on macOS host; the BufReader change is NOT the cause (revert experiment); needs Linux/CI green | Form 8 |
| ft-ui1xn remote-cv artifact | **recorded** — the RCH-remote auto-VERDICT `promote=false` is a noisy-host cv artifact, not a refutation (Mac cleared cv≤5%) | Form 7 |
| speculative new-axis mining | **not attempted** (operator: targeted-finish; recommend against) | Form 1 |

## Release evidence

- **Version bump:** workspace-root `Cargo.toml` `[workspace.package]` `0.10.1 → 0.10.2` only (portable-pty
  stays independent at 0.9.0). `SOURCE_DATE_EPOCH=0` clean stamp; build.rs skips the git-dirty check under it.
- **Commits:** `9137b11ab` (ft-ui1xn+ft-zhj63), `dd0043b79` (ft-yjihu.1 WAL), `213938050` (beads),
  `6f8089935` (scan_pipeline deletion), + ledgers/campaign/changelog/version commit.
- **Build:** darwin-arm64 local (full default-members for the `.app`); linux amd64 native + arm64 cross on
  the Contabo fleet (`gcc-` AND `g++-aarch64-linux-gnu` + `CXX_aarch64` for esaxx-rs); windows amd64 (`ft.exe`)
  on the Tailscale host — coordinated with the operator (prepare-on-main).
- **Assets must match `install.sh`:** `ft-{darwin-arm64,linux-amd64,linux-arm64}.tar.xz` +
  `FrankenTerm-darwin-arm64.app.tar.xz` + `ft-windows-amd64.zip` + per-asset `.sha256` + `SHA256SUMS`.

## Convergence declaration

The optimization campaign is **FULLY converged.** Five rounds (v0.7→v0.10.2) promoted default-on wins across
every genuinely-hot production path (Q1 prefix-sum 20–32×, EV4 set-based FTS 6–14×, the dense-ASCII
term-render stack 4.4×, adaptive-M4 CDC −80% RSS) and built a load-bearing negative-evidence moat. Round-9
closed the last two live threads — the largest single detection-path win (removing the net-negative
`quick_reject` prefilter, 22.76% of fleet self-time) and the WAL startup lever (+74%) — caught one false-open
(ft-uyt88), and removed 4260 lines of dead `scan_pipeline` code. **The well is dry.** Future effort should
pivot to product work — the Windows port (`ft-azsnz`), the mlua P1 (`ft-47z57`), or the ft-uyt88 Linux/CI
verification — not further per-op micro-mining.
