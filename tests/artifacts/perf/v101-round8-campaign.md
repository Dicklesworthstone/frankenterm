# v0.10.1 — Round-8 Alien Optimization Gauntlet Campaign Record — FINAL

> **v0.10.1 SHIPPED** — 3 platforms + checksums:
> https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.1 (tag `v0.10.1`, commit `7fb968b17`).
> Autonomous orchestrator, lean execution (no swarm needed for a convergence round). Resumed post-v0.10.0
> (`5baf072f9`). Ledgers: `docs/perf-ledger/round8-{keep-ledger,negative-results}.md`. This is the
> **round-8 scorecard and the campaign's convergence declaration.**

## Charter (operator-locked)

- **Scope:** convergence / consolidation — ship certified carryover + implement the one profiled lever +
  hygiene; declare the optimization campaign substantially converged. **No speculative new mining.**
- **Cadence:** autonomous to release.
- **End state:** cut **v0.10.1** (patch).
- **Bench host:** local Mac + deterministic harnesses; correctness proofs RCH-remote / fail-closed.

## The distilled 4-round verdict (drove the round)

After v0.7→v0.10, the per-op CPU micro-space is **exhausted**. The only pattern that ever delivered:
replace per-element work with a bulk/batch/single-pass op on a genuinely-live hot frame, OR collapse a
complexity class. Round-7's B0′ profile found **exactly one** new CPU frame clearing both the ≥0.5% gate
AND production-liveness (startup WAL recovery). Everything else is pre-rejected (see
`round8-negative-results.md` PRE-REJECTED list). Hence: convergence, not mining.

## Workstreams

| Item | Class | Status |
|---|---|---|
| Ship adaptive-M4 CDC dedup (certified, unshipped after round-7) | release carryover | **SHIPPED** in v0.10.1 (default-on, −80% RSS) |
| WAL skip-checkpoint lever (`ft-yjihu.1`, the one profiled+live frame) | new lever | **LANDED default-off** (`70ee7c9dd`); 8/8 RCH-remote proof |
| README `scan_pipeline` cross-chunk reality-gap (`ft-z91oa`) | hygiene/docs | **FIXED** (`f1bc1c975`) |
| `ft-ui1xn` quick_reject vs ac_direct A/B (Form-8 dep now satisfied) | hygiene/measure | **measured — carried to round-9** (promising, profile-gated) |
| `scan_pipeline` deletion | hygiene | **deferred to round-9** (operator) |
| Windows (x86_64-pc-windows-msvc) asset | release platform | **SHIPPED** — `ft.exe` from `8bdf23979`; "hard wall" was a stale host nightly (rustup update) + 8 cfg-gating fixes |
| Cut v0.10.1 | release | **SHIPPED** (`v0.10.1` / `7fb968b17` unix + `8bdf23979` windows, 4 platforms + checksums) |

## Scorecard

### Shipped → default-on
| Idea | Win | Proof |
|---|---|---|
| adaptive-M4 CDC scrollback dedup | **−80.14% fleet RSS** on redundant redraws (27.9→5.5 MB @200 panes); low-redundancy +0.00% TIE | round-7 cert (`557982cb7`): RSS harness `vmi1227854` + `proptest_scrollback_cdc_dedup` 4 passed. Round-8 action = ship it. |

### Landed → default-off (kept, promotion-pending)
| Idea | Bead / commit | Win | Proof |
|---|---|---|---|
| WAL skip-startup-checkpoint | ft-yjihu.1 / `70ee7c9dd` | structural — skips ~8 ms startup checkpoint on a small dirty WAL (round-7: 3.528% startup self-time) | RCH-remote `vmi1227854 (1219.4s)` fail-closed, `round8_wal_recovery` **8 passed/0 failed**: durability oracle (no row loss when skipped), corruption fail-closed under both gate states, end-to-end env→gate wiring |

### Measured / carried / deferred (negative evidence = a win)
| Idea | Status | Retry form |
|---|---|---|
| ft-ui1xn quick_reject vs ac_direct | **carryover** — ac_direct faster at 1–16KB (synthetic no-match), tied at 64KB; profile-gate (`detect_with_context` ≥0.5%) UNMET; not refuted, not promoted; quick_reject stays default-on | Form 8 + 1 (gated on `ft-zhj63` + realistic-workload A/B at cv≤5%) |
| scan_pipeline deletion | **deferred to round-9** (operator); README reality-gap fixed so docs no longer mislead | Form 2 |
| Windows build | **RESOLVED / SHIPPED** (operator follow-up) — `ft.exe` builds for x86_64-pc-windows-msvc; the `cfg_select` "wall" was a stale host nightly (`rustup update nightly`), plus portable-pty / openssl-vendored / signal-alias / cfg-gating fixes (`91f8483d7`+`8bdf23979`); green Linux check confirmed no unix regression | n/a (resolved) |

### Release evidence
- **Tag/commit:** `v0.10.1` / `7fb968b17` (clean tree; `SOURCE_DATE_EPOCH=0` clean stamp — `ft --version` →
  `ft 0.10.1 (7fb968b17)`, `built: 1970-01-01`, no `+dirty`).
- **Build:** darwin-arm64 local (full default-members for the `.app`); linux amd64 native + arm64 cross
  (`gcc-aarch64-linux-gnu` **and** `g++-aarch64-linux-gnu` + `CXX_aarch64_unknown_linux_gnu` — the C++
  cross compiler is required for the esaxx-rs dep, a round-8 learning) on Contabo `vmi1227854`; windows
  amd64 (`ft.exe`, `--no-default-features` minus jemalloc, vendored OpenSSL via Strawberry Perl) on the
  Tailscale host `surfacebookje`/`wlap` from `8bdf23979`.
- **Assets (names match `install.sh`):** `ft-{darwin-arm64,linux-amd64,linux-arm64}.tar.xz` +
  `FrankenTerm-darwin-arm64.app.tar.xz` + `ft-windows-amd64.zip` + per-asset `.sha256` + `SHA256SUMS`.
  All 5 checksums verified.
- **Version bump:** only workspace-root `Cargo.toml` (`0.10.0`→`0.10.1`) + a `Cargo.lock` member-version
  sync; portable-pty stays at its independent `0.9.0`. Pushed `main` + `main:master`. Auto-triggered
  `release.yml` cancelled (the established dsr-manual convention, as for v0.9.0/v0.10.0).

## Convergence declaration

The optimization campaign is **substantially converged.** Four rounds drove default-on promotions across
the genuinely-hot paths (Q1 prefix-sum, EV4 set-based FTS, the dense-ASCII term-render stack, adaptive-M4
CDC) and built a load-bearing negative-evidence moat that pre-rejects every exhausted micro-class. The
single remaining profiled+live CPU lever (WAL skip-checkpoint) landed this round. **One live carryover
remains for an optional round-9:** `ft-ui1xn` (quick_reject vs ac_direct), which showed a promising A/B
signal but is profile-gated on `ft-zhj63`. Recommended pivot for future effort: the Windows port (its own
epic) and the live non-campaign hot-path beads (`ft-gkh4p` redaction double-scan, `ft-4lq0i` redactor
multi-pass, `ft-uyt88` mux reader buffering), rather than further per-op micro-mining.
