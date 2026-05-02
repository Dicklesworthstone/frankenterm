# Handoff docs

Per-session handoff notes from the swarm. Each agent that runs a
substantive cycle drops a `<agent>-session-<date>.md` here so the
next agent can resume without re-deriving context.

## Index

- [cc_2-session-2026-05-02.md](./cc_2-session-2026-05-02.md) —
  19 deliverables across 14 beads (atlas substrate completion,
  Profile family handler, LabRuntime nightly CI lane, JSONL
  telemetry, doctor surfaces, RPC envelope types, regex catalog,
  storage callsite analyzer + recipe guide + 2 CI gates,
  storage_convert example, Row accessor substrate, blit budget
  calculator, operator playbooks, demo-full preflight verifier,
  per-PR diff helper). 7 closed + 8 with substrate progress.

## Conventions

- File name: `<agent_id>-session-<YYYY-MM-DD>.md`. When an agent
  runs across midnight UTC, use the date the session started.
- Top of file: a one-line summary + closed/open bead counts so
  the index above can stay terse.
- Body: the substrate-shipped table (bead | slice | commit |
  deferred wired-pass) is load-bearing; every other section is
  optional but recommended.
- "What the next agent picks up" section: ordered by impact,
  pointing at the highest-leverage wired-pass slices.
- Disk-pressure notes: when the session navigated emergency
  cycles, document the strategy that kept the swarm alive
  (which dirs were safe to remove, which were not).
