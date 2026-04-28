# 2026-04-28 — cc_1 audit-rotation session

Single-agent audit rotation across the audit-skill family. Each skill applied to `crates/frankenterm-core/`, capped at 5 findings or 30 min per pass.

## (1) Audit-filed beads by skill

| Skill | Filed | Bead IDs |
|-------|------:|----------|
| reality-check | 2 | ft-xaych, ft-ijbql |
| conformance | 5 | ft-5ikbd, ft-2sumi, ft-d5268, ft-b35tw, ft-zaqi8 |
| golden-artifacts | 5 | ft-wvo1p, ft-nvr6a, ft-5490a, ft-40k1o, ft-yex2c |
| fuzzing | 5 | ft-h8v8v, ft-8hbq8, ft-ul4vi, ft-hfbsp, ft-s96ej |
| profiling | 5 | ft-0zoq3, ft-9r82k, ft-o2mtn, ft-f6mu7, ft-3r0n4 |
| modes-of-reasoning | 5 | ft-dn2tu, ft-3tvvt, ft-8nqx0, ft-bzgxi, ft-k3y0u |
| security-audit | 5 | ft-3xek9, ft-nrqf7, ft-j0ufc, ft-3se13, ft-0ctwe |
| multi-pass-bug-hunting | 4 | ft-xvrlp, ft-o2t7l, ft-oqfsx, ft-qkd2f |
| codebase-audit | 5 | ft-ctt7k, ft-7v53r, ft-pe4ds, ft-3p7re, ft-5cl1b |
| **total** | **41** | + ft-zd6cx (drift follow-on surfaced by ft-2sumi) |

## (2) Implementation beads cc_1 shipped

ft-v5lz3.1.1 / .1.2 / .1.3 (mem-gc trio) → ft-v5lz3.1 parent · ft-v5lz3.1.4 (`scripts/memory_staleness_check.py`) · ft-v5lz3.3 (AGENTS.md SO-playbook) · ft-v5lz3.2.4 (`docs/operator-runbook.md`, 12 KB) · ft-v5lz3.2.3 (CI shellcheck + bats macos+ubuntu) · ft-v5lz3.2.6 / .2.7 (Linux portability for clean-stale + swarm-tick) → ft-v5lz3.2 parent · ft-ombfl.1 (GPU regression harness ADR, 310 lines) · ft-ombfl.10 (frame-time reporter) · ft-5ikbd (runtime envelope schema validator, 5 tests + 14 envelopes validated) · ft-2sumi (ft.toml conformance — 9 tests, surfaced ft-zd6cx drift) · wa-nu4.3.9 audit comment. ft-ao9k9 claimed but released-with-blocker note (linter revert + pre-existing E0063).

## (3) Top 3 highest-leverage findings for next session

1. **ft-3xek9** [security] — Redactor blocklist misses xAI/Groq/Cohere/raw-JWT-without-bearer; **direct credential leak** through `ft robot get-text` + MCP. Narrowly-scoped fix (5 prefix patterns + raw-JWT fallback + provider-matrix gate).
2. **ft-3se13** [security] — Policy decision log writes `command_text` unredacted; redactor IS wired to `AuditActionRecord` but NOT to the decision log. **Years of operator-typed plaintext secrets** in the audit DB. One-function fix.
3. **ft-qkd2f** [multi-pass] — `PaneLockGuard` scope-guard refactor replaces 14 manual `release(...)` sites with Drop-based release. **Type-system enforcement of the lock-leak invariant**; foundation for ft-ao9k9, ft-3p7re, ft-j0ufc workflow follow-ons.

## (4) Audits that surfaced ZERO new defects (positive signal)

- **FTS5 query injection** (security-audit candidate) — production code uses `?`-bound parameters at all six MATCH sites; verified safe. The `format!("…IN ({placeholders})")` interpolates only `?` repetition, not data. Proper parameterization across the search surface.
- **`storage.rs` first 19,499 production lines** (multi-pass) — **zero `.lock().unwrap()`** and zero production unwraps. Exceptionally well-defended for a 34K-line file.
- **Tx state machine** (multi-pass Pass 4) — `transition_phase` is gated by `can_transition_to`; tests at `tx_idempotency.rs:1084-1087` pin the legal transitions. Clean state-machine discipline.
- **`recorder_storage.rs` first 1,234 production lines** — zero unwraps, zero `.ok()` swallows. Mirror of the storage.rs defense.

These zero-findings are themselves a useful signal: the storage / recorder / Tx surfaces are the load-bearing data-correctness paths in ft and they're holding the bar.
