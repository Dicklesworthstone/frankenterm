# Postswarm Summary — 2026-04-27 Swarm Session

This document is the changelog-style summary for the work that
followed the 2026-04-27 multi-agent swarm session, tracked under
[ft-v5lz3](#) (postswarm-2026-04-27 follow-ups).

## The session itself (1h45m, 2026-04-27 18:48Z → 20:38Z)

- **Composition**: 2 cc panes + 4 cod panes + 1 user pane + 1
  operator agent driving 4-minute ticks.
- **Throughput**: 35 commits, 16 beads closed.
- **Initial ready set**: 4 beads (ft-1memj.28, wa-nu4.3.9, ft-6scm7,
  ft-z1809).
- **FINAL exploratory pass**: dispatched one of
  `/security-audit-for-saas`, `/modes-of-reasoning-project-analysis`,
  `/profiling-software-performance`, `/deadlock-finder-and-fixer`,
  `/mock-code-finder`, `/reality-check-for-project` per pane. That
  pass filed 13 new beads, almost all of which closed within the
  same session (ft-lv819, ft-psa5d, ft-9ey46, ft-jehxh, ft-besoo,
  ft-okhhj, ft-iqwt5, ft-xg8st, ft-hdvvo, ft-d3awp, ft-mt6uv,
  ft-thj9b, ft-h35n4, ft-npobg, ft-5o6u5).

## Tracking-epic deliverables (ft-v5lz3 children)

| Child | Title | Status | Key landings |
|------|------|--------|--------------|
| ft-v5lz3.1 | GC three stale subsystem-sweep memories from MEMORY.md | ✓ | All 3 sub-children verified at HEAD found 0 cat-A; memory files deleted; .1.4 added an automation script to prevent recurrence |
| ft-v5lz3.2 | Harden operator helpers — tests + shellcheck + runbook + ntm-coordinator | ✓ | 8 children closed (3 follow-ons beyond original 5), 46 bats tests, shellcheck-clean on operator family, 12.3KB operator runbook, CI hard-gate on macos-14 + ubuntu-latest, Linux portability for swarm-tick + clean-stale, cross-script concurrency safety |
| ft-v5lz3.3 | AGENTS.md addendum — Swarm Orchestration Playbook | ✓ | 8 rules SO-1..SO-8 landed at e289e84c between "Related Tools" and "Testing"; cross-links vibing-with-ntm + ntm skills + operator helper scripts |
| ft-v5lz3.4 | Audit wa-nu4.3.9 vs delivered commits | ✓ | Scope audited, evidence table appended, CI/release proof children filed, epic released unassigned |
| ft-v5lz3.5 | Final wrap-up | ✓ (this commit) | Retrospective + summary docs landed; ROOT closed |

## Operator-tooling additions (ft-v5lz3.2 family)

- `scripts/swarm-tick.sh` — operator periodic tick driver (now Linux-portable)
- `scripts/clean-stale-targets.sh` — clean stale `/tmp/ft-*-target/` dirs (now Linux-portable, skips active dirs)
- `docs/operator-runbook.md` — 12.3KB / 8 sections + 10-line Quick Reference, cross-linked from AGENTS.md
- CI: shellcheck severity=error on `scripts/*.sh`; bats operator-shell-tests job on macos-14 + ubuntu-latest

## AGENTS.md addendum (ft-v5lz3.3)

The "Swarm Orchestration Playbook" section codifies 8 empirically-
validated rules from this session:

- **SO-1**: Prefer `--robot-send` over `--robot-interrupt --interrupt-msg` for cooperative agents (codex pane crash repro at tick #11)
- **SO-2**: Always send tmux Enter after `ntm --robot-send` (twice for codex, ~2s apart)
- **SO-3**: Codex idle-placeholder text is not stuck-pane evidence
- **SO-4**: CC convergence language is explicit; codex convergence is silent
- **SO-5**: `commits-1h ≤ 2` lags real convergence by ~45 min — tighten to `≤ 4`
- **SO-6**: Disk pressure is manageable with per-agent target dirs + completion cleanup
- **SO-7**: Use `ntm --robot-send` for repeated nudges; `ntm send` is CASS-deduped
- **SO-8**: Long-running in_progress beads — broadcast first, force-release only on silence

## Cross-references

- Retrospective: [`docs/postswarm-2026-04-27-retro.md`](postswarm-2026-04-27-retro.md)
- Operator runbook: [`docs/operator-runbook.md`](operator-runbook.md)
- AGENTS.md Swarm Orchestration Playbook section
